use super::*;
use crate::ir::{BinaryOp, Redirects, StderrTarget, WriteMode};
use crate::path::TildePath;

#[test]
fn synthetic_names_are_outside_the_identifier_alphabet() {
    let name = synthetic("eta", 1);
    assert!(is_synthetic(&name));
    assert!(!crate::syntax::lexer::is_ident(&name));
    assert!(!is_synthetic("_variant"));
}

// ── referenced_names: exhaustive walker coverage ─────────────────────

fn var(name: &str) -> Val {
    Val::Variable(name.into())
}

fn svar(name: &str) -> Spanned<Val> {
    Spanned::synthetic(var(name))
}

fn ret(name: &str) -> Arc<Comp> {
    Arc::new(Spanned::synthetic(CompKind::Return(var(name))))
}

/// IR forms: `r_*` labels references, `*_bound` mere binders.
pub(crate) fn every_variant() -> Vec<Arc<Comp>> {
    let lam_param = "lam_param_bound".into();
    let lam = Spanned::synthetic(CompKind::Lam {
        param: lam_param,
        body: ret("r_lam_body"),
    });

    let bind_pattern = Pattern::List {
        elems: vec![Pattern::Name("bind_map_bound".into())],
        rest: Some("bind_rest_bound".into()),
    };
    let bind = Spanned::synthetic(CompKind::Bind {
        comp: ret("r_bind_comp"),
        hole: (),
        pattern: Arc::new(bind_pattern),
        rest: ret("r_bind_rest"),
    });

    let seq = Spanned::synthetic(CompKind::Seq {
        first: ret("r_seq_first"),
        rest: ret("r_seq_rest"),
    });

    let app = Spanned::synthetic(CompKind::App {
        head: Arc::new(Spanned::synthetic(CompKind::Force(var("r_app_head")))),
        args: Box::new([svar("r_app_arg_first"), svar("r_app_arg_second")]),
        hole: (),
    });

    let exec_name = Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::Name(CommandName::Bare("r_exec_name_head".into())),
        argv: vec![
            ValListElem::Single(svar("r_exec_arg")),
            ValListElem::Spread(svar("r_exec_arg_spread")),
        ]
        .into(),
        redirects: Redirects {
            stdout: Some((WriteMode::Write, var("r_exec_redirect_target"))),
            ..Redirects::default()
        },
        site: None,
    }));
    let exec_external = Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::External(CommandName::Bare("r_exec_external_head".into())),
        argv: Argv::default(),
        // A dup has no operand, so contributes no reference to over-collect.
        redirects: Redirects {
            stderr: Some(StderrTarget::Stdout),
            ..Redirects::default()
        },
        site: None,
    }));

    let pipeline = Spanned::synthetic(CompKind::Pipeline {
        stages: vec![
            Arc::new(Spanned::synthetic(CompKind::Force(var(
                "r_pipeline_stage1",
            )))),
            Arc::new(Spanned::synthetic(CompKind::Force(var(
                "r_pipeline_stage2",
            )))),
        ],
    });

    let binary = Spanned::synthetic(CompKind::Prim(Prim::Binary(
        BinaryOp::Arith(ArithOp::Add),
        var("r_binary_a"),
        var("r_binary_b"),
    )));
    let negate = Spanned::synthetic(CompKind::Prim(Prim::Negate(var("r_negate"))));
    let not = Spanned::synthetic(CompKind::Prim(Prim::Not(var("r_not"))));
    let render = Spanned::synthetic(CompKind::Prim(Prim::Render(var("r_render"))));
    let index = Spanned::synthetic(CompKind::Prim(Prim::Index {
        target: var("r_index_target"),
        key: Spanned::synthetic(Key::Data(var("r_index_key"))),
    }));
    let index_label = Spanned::synthetic(CompKind::Prim(Prim::Index {
        target: var("r_index_target"),
        key: Spanned::synthetic(Key::Label("index_label".into())),
    }));
    let index_position = Spanned::synthetic(CompKind::Prim(Prim::Index {
        target: var("r_index_target"),
        key: Spanned::synthetic(Key::Position(0)),
    }));
    let concat = Spanned::synthetic(CompKind::Prim(Prim::Concat(
        vec![var("r_concat_a"), var("r_concat_b")].into(),
    )));
    let rec_group: Arc<GroupNode> =
        Node::new(vec![("rec_name_bound".into(), ret("r_rec_member"))].into());
    let rec = Spanned::synthetic(CompKind::Rec {
        group: rec_group,
        index: 0,
    });
    let tilde = Spanned::synthetic(CompKind::Tilde(TildePath { suffix: None }));
    let if_ = Spanned::synthetic(CompKind::If {
        cond: Spanned::synthetic(var("r_if_cond")),
        then: svar("r_if_then"),
        else_: svar("r_if_else"),
    });
    let case = Spanned::synthetic(CompKind::Case {
        scrutinee: Spanned::synthetic(var("r_case_scrutinee")),
        arms: vec![CaseArm {
            tag: Spanned::synthetic("some".into()),
            body: svar("r_case_arm_body"),
        }],
    });

    let scope_try = Spanned::synthetic(CompKind::Try {
        body: var("r_try_body"),
        handler: var("r_try_handler"),
    });
    let scope_guard = Spanned::synthetic(CompKind::Guard {
        acquire: var("r_guard_acquire"),
        hole: (),
        body: var("r_guard_body"),
        release: var("r_guard_release"),
    });
    let scope_within = Spanned::synthetic(CompKind::Within {
        opts: Node::new(
            vec![
                ("dir".into(), svar("r_within_opts")),
                (
                    "handlers".into(),
                    Spanned::synthetic(Val::map(vec![("deploy".into(), svar("r_within_arm"))])),
                ),
            ]
            .into(),
        ),
        body: var("r_within_body"),
    });
    let scope_grant = Spanned::synthetic(CompKind::Grant {
        caps: Node::new(vec![("net".into(), svar("r_grant_caps"))].into()),
        body: var("r_grant_body"),
    });
    let scope_audit = Spanned::synthetic(CompKind::Audit {
        body: var("r_audit_body"),
    });
    let scope_spawn = Spanned::synthetic(CompKind::Spawn {
        title: Some(svar("r_spawn_title")),
        body: var("r_spawn_body"),
    });
    let scope_gather = Spanned::synthetic(CompKind::Gather {
        body: var("r_gather_body"),
    });
    let scope_redirect = Spanned::synthetic(CompKind::Redirect {
        body: ret("r_scope_redirect_body"),
        redirects: Redirects {
            stderr: Some(StderrTarget::File(
                WriteMode::Append,
                var("r_scope_redirect_target"),
            )),
            ..Redirects::default()
        },
    });
    let val_list = Spanned::synthetic(CompKind::Return(Val::list(vec![
        Spanned::synthetic(Val::Unit),
        Spanned::synthetic(Val::String("s".into())),
        Spanned::synthetic(Val::Int(1)),
        Spanned::synthetic(Val::Float(Finite::new(1.0).expect("finite"))),
        Spanned::synthetic(Val::Bool(true)),
        Spanned::synthetic(Val::Bytes(vec![0, 128, 255].into())),
        svar("r_list_single"),
    ])));
    let val_record = Spanned::synthetic(CompKind::Return(Val::record(vec![(
        "lbl".into(),
        svar("r_record_value"),
    )])));
    let val_map = Spanned::synthetic(CompKind::Return(Val::map(vec![(
        "lbl".into(),
        svar("r_map_value"),
    )])));
    let assemble_list = Spanned::synthetic(CompKind::Prim(Prim::List(Box::new([
        ValListElem::Single(svar("r_assemble_list_single")),
        ValListElem::Spread(svar("r_assemble_list_spread")),
    ]))));
    let update = Spanned::synthetic(CompKind::Prim(Prim::Update {
        base: svar("r_update_base"),
        fields: Box::new([("lbl".into(), svar("r_update_field"))]),
    }));
    let assemble_map = Spanned::synthetic(CompKind::Prim(Prim::Map(Box::new([
        MapEntryV::Entry(svar("r_assemble_map_key"), svar("r_assemble_map_value")),
        MapEntryV::Spread(svar("r_assemble_map_spread")),
    ]))));
    let val_variant = Spanned::synthetic(CompKind::Return(Val::Variant {
        label: "lbl".into(),
        payload: Box::new(var("r_variant_payload")),
    }));
    let val_variant_empty = Spanned::synthetic(CompKind::Return(Val::Variant {
        label: "lbl_empty".into(),
        payload: Box::new(Val::Unit),
    }));
    let val_thunk = Spanned::synthetic(CompKind::Return(Val::thunk(Arc::new(Spanned::synthetic(
        CompKind::Return(var("r_thunk_body")),
    )))));
    let capture = Spanned::synthetic(CompKind::Capture(ret("r_capture_body")));
    let decode = Spanned::synthetic(CompKind::Prim(Prim::Decode(var("r_decode_body"))));
    let use_module = Spanned::synthetic(CompKind::Use(Arc::new(Module {
        top: toplevel(vec![run(ret("module_own_reference"))]),
        origin: SourceOrigin::named("linked module"),
    })));

    let builtin = Spanned::synthetic(CompKind::Builtin {
        builtin: "r_builtin".into(),
        args: vec!["r_builtin_arg_first".into(), "r_builtin_arg_second".into()].into(),
    });

    // No single `CompKind` holds an arbitrary list of sub-`Comp`s to
    // wrap all of the above in one tree, so each is walked on its own
    // and the harvests are unioned.
    vec![
        Arc::new(Spanned::synthetic(CompKind::Force(var("r_force")))),
        Arc::new(Spanned::synthetic(CompKind::Return(var("r_return")))),
        Arc::new(lam),
        Arc::new(bind),
        Arc::new(seq),
        Arc::new(app),
        Arc::new(exec_name),
        Arc::new(exec_external),
        Arc::new(pipeline),
        Arc::new(binary),
        Arc::new(negate),
        Arc::new(not),
        Arc::new(render),
        Arc::new(index),
        Arc::new(index_label),
        Arc::new(index_position),
        Arc::new(concat),
        Arc::new(rec),
        Arc::new(tilde),
        Arc::new(if_),
        Arc::new(case),
        Arc::new(scope_try),
        Arc::new(scope_guard),
        Arc::new(scope_within),
        Arc::new(scope_grant),
        Arc::new(scope_audit),
        Arc::new(scope_spawn),
        Arc::new(scope_gather),
        Arc::new(scope_redirect),
        Arc::new(val_list),
        Arc::new(val_record),
        Arc::new(val_map),
        Arc::new(assemble_list),
        Arc::new(update),
        Arc::new(assemble_map),
        Arc::new(val_variant),
        Arc::new(val_variant_empty),
        Arc::new(val_thunk),
        Arc::new(capture),
        Arc::new(decode),
        Arc::new(use_module),
        Arc::new(builtin),
    ]
}

#[test]
fn wire_variants_round_trip_and_delta_does_not_serialize() {
    for node in &every_variant() {
        let bytes = postcard::to_allocvec(node);
        if matches!(node.item, CompKind::Builtin { .. }) {
            assert!(bytes.is_err(), "δ is not wire syntax");
        } else {
            let decoded: Arc<Comp> =
                postcard::from_bytes(&bytes.expect("serialize IR")).expect("deserialize IR");
            assert_eq!(node, &decoded);
        }
    }
}

/// The harvest is asserted *exactly* — a subset would hide a wildcard-arm
/// regression, a superset a bound name over-renewing.
#[test]
fn referenced_names_walks_every_variant() {
    let nodes = every_variant();
    let found: std::collections::HashSet<&str> = nodes
        .iter()
        .flat_map(|c| {
            let mut out = Vec::new();
            c.mentions(&mut out);
            out.into_iter().map(AsRef::as_ref).collect::<Vec<_>>()
        })
        .collect();

    let expected = [
        "r_force",
        "r_return",
        "r_lam_body",
        "r_bind_comp",
        "r_bind_rest",
        "r_seq_first",
        "r_seq_rest",
        "r_app_head",
        "r_app_arg_first",
        "r_app_arg_second",
        "r_exec_name_head",
        "r_exec_arg",
        "r_exec_arg_spread",
        "r_exec_redirect_target",
        "r_exec_external_head",
        "r_pipeline_stage1",
        "r_pipeline_stage2",
        "r_binary_a",
        "r_binary_b",
        "r_negate",
        "r_not",
        "r_render",
        "r_index_target",
        "r_index_key",
        "r_concat_a",
        "r_concat_b",
        "r_rec_member",
        "r_if_cond",
        "r_if_then",
        "r_if_else",
        "r_case_scrutinee",
        "r_case_arm_body",
        "r_try_body",
        "r_try_handler",
        "r_guard_acquire",
        "r_guard_body",
        "r_guard_release",
        "r_within_opts",
        "r_within_arm",
        "r_within_body",
        "r_grant_caps",
        "r_grant_body",
        "r_audit_body",
        "r_spawn_title",
        "r_spawn_body",
        "r_gather_body",
        "r_scope_redirect_body",
        "r_scope_redirect_target",
        "r_list_single",
        "r_record_value",
        "r_map_value",
        "r_assemble_list_single",
        "r_assemble_list_spread",
        "r_update_field",
        "r_update_base",
        "r_assemble_map_key",
        "r_assemble_map_value",
        "r_assemble_map_spread",
        "r_variant_payload",
        "r_thunk_body",
        "r_capture_body",
        "r_decode_body",
        "r_builtin",
        "r_builtin_arg_first",
        "r_builtin_arg_second",
    ];

    for name in expected {
        assert!(found.contains(name), "missing reference: {name}");
    }
    assert_eq!(
        found.len(),
        expected.len(),
        "unexpected extra name in {found:?}; a bound-not-referenced name \
         (`*_bound`) must be absent"
    );
}

/// A thunk mentioning `b`, `a`, `b` and nesting a thunk over `c`, `a` has
/// occ `[a, b, c]`: sorted, distinct, the nested node's names included.
#[test]
fn occ_is_sorted_distinct_and_includes_nested_nodes() {
    let inner = Val::thunk(Arc::new(Spanned::synthetic(CompKind::Prim(Prim::Binary(
        BinaryOp::Arith(ArithOp::Add),
        var("c"),
        var("a"),
    )))));
    let outer = ThunkNode::new(Arc::new(Spanned::synthetic(CompKind::Prim(Prim::Concat(
        vec![var("b"), var("a"), var("b"), inner].into(),
    )))));
    let occ: Vec<&str> = outer.occ().names().map(|n| &**n).collect();
    assert_eq!(occ, ["a", "b", "c"]);
}

/// A list node serialises to its shape alone, and decodes with the same
/// occ it was built with.
#[test]
fn a_node_crosses_the_wire_as_its_shape() {
    let node = ListNode::new(vec![svar("a"), svar("a"), svar("b")].into());
    let wire = serde_json::to_string(&node).expect("serialize node");
    let shape_wire = serde_json::to_string(node.shape()).expect("serialize shape");
    assert_eq!(wire, shape_wire, "the wire carries the shape alone");

    let decoded: Arc<ListNode> = serde_json::from_str(&wire).expect("deserialize node");
    assert_eq!(decoded.occ(), node.occ());
}

// ── Argv, Toplevel, Pattern, Redirects ───────────────────────────────

fn exec() -> Arc<Comp> {
    Arc::new(Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::Name(CommandName::Bare("ls".into())),
        argv: Argv::default(),
        redirects: Redirects::default(),
        site: None,
    })))
}

fn run(comp: Arc<Comp>) -> Spanned<Phrase> {
    Spanned::synthetic(Phrase::Run(comp))
}

fn define(name: &str, ty: crate::ty::Ty) -> Spanned<Phrase> {
    Spanned::synthetic(Phrase::Define {
        pattern: Arc::new(Pattern::Name(name.into())),
        comp: ret("v"),
        hole: (),
        schemes: vec![(name.into(), crate::ty::Binders::close(|_| ty))],
    })
}

fn toplevel(phrases: Vec<Spanned<Phrase>>) -> Toplevel {
    Toplevel {
        phrases,
        admits: vec![],
    }
}

#[test]
fn the_last_let_of_a_name_wins_the_exported_scheme() {
    use crate::ty::{Binders, Ty};
    let unit = toplevel(vec![
        define("x", Ty::int()),
        run(exec()),
        define("y", Ty::string()),
        define("x", Ty::bool()),
    ]);
    let scheme = |ty| Binders::close(|_| ty);
    assert_eq!(
        unit.exported_schemes(),
        [
            ("x".to_string(), scheme(Ty::bool())),
            ("y".to_string(), scheme(Ty::string()))
        ]
    );
}

fn redirects() -> Redirects<u8> {
    Redirects {
        stdin: Some(StdinSource::Here(1)),
        stdout: Some((WriteMode::Write, 2)),
        stderr: Some(StderrTarget::File(WriteMode::Append, 3)),
    }
}

#[test]
fn redirect_operands_come_in_opening_order_and_a_dup_has_none() {
    assert_eq!(
        redirects().operands().copied().collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let dup = Redirects {
        stderr: Some(StderrTarget::Stdout),
        ..redirects()
    };
    assert_eq!(dup.operands().copied().collect::<Vec<_>>(), [1, 2]);
    assert_eq!(Redirects::<u8>::default().operands().count(), 0);
}

#[test]
fn try_map_visits_operands_in_opening_order_and_stops_at_the_first_error() {
    let mut seen = Vec::new();
    let doubled = redirects().try_map(|&t| {
        seen.push(t);
        Ok::<_, u8>(t * 2)
    });
    assert_eq!(
        doubled,
        Ok(Redirects {
            stdin: Some(StdinSource::Here(2)),
            stdout: Some((WriteMode::Write, 4)),
            stderr: Some(StderrTarget::File(WriteMode::Append, 6)),
        })
    );
    assert_eq!(seen, [1, 2, 3]);

    seen.clear();
    let failed = redirects().try_map(|&t| {
        seen.push(t);
        if t == 2 { Err(t) } else { Ok(t) }
    });
    assert_eq!(failed, Err(2));
    assert_eq!(seen, [1, 2], "stderr is not reached");
}
