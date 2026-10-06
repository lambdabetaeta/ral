use super::*;
use crate::ir::{BinaryOp, Redirects, StderrTarget, WriteMode};
use crate::path::tilde::TildePath;

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

/// One synthetic `Comp` per `CompKind` and `Val` variant: `r_*` labels
/// what it references, `*_bound` what it merely binds.  The harvest is
/// asserted *exactly* — a subset would hide a wildcard-arm regression, a
/// superset a bound name over-renewing.
#[test]
fn referenced_names_walks_every_variant() {
    let lam_param = Pattern::Name("lam_param_bound".into());
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
        pattern: Arc::new(bind_pattern),
        rest: ret("r_bind_rest"),
    });

    let app = Spanned::synthetic(CompKind::App {
        head: Arc::new(Spanned::synthetic(CompKind::Force(var("r_app_head")))),
        args: vec![
            ValListElem::Single(svar("r_app_arg_single")),
            ValListElem::Spread(svar("r_app_arg_spread")),
        ]
        .into(),
    });

    let exec_name = Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::Name(CommandName::Bare("r_exec_name_head".into())),
        args: vec![ValListElem::Single(svar("r_exec_arg"))].into(),
        redirects: Redirects {
            stdout: Some((WriteMode::Write, var("r_exec_redirect_target"))),
            ..Redirects::default()
        },
        site: None,
    }));
    let exec_external = Spanned::synthetic(CompKind::Exec(Exec {
        head: CommandWord::External(CommandName::Bare("r_exec_external_head".into())),
        args: Args::default(),
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

    let binary = Spanned::synthetic(CompKind::Binary(
        BinaryOp::Arith(ArithOp::Add),
        var("r_binary_a"),
        var("r_binary_b"),
    ));
    let not = Spanned::synthetic(CompKind::Not(var("r_not")));
    let index = Spanned::synthetic(CompKind::Index {
        target: var("r_index_target"),
        keys: vec![Spanned::synthetic(var("r_index_key"))],
    });
    let interpolation = Spanned::synthetic(CompKind::Interpolation(vec![
        var("r_interp_a"),
        var("r_interp_b"),
    ]));
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
        body: var("r_guard_body"),
        cleanup: var("r_guard_cleanup"),
    });
    let scope_within = Spanned::synthetic(CompKind::Within {
        opts: vec![("dir".into(), svar("r_within_opts"))].into(),
        handlers: Some(vec![HandlerArmV {
            name: "deploy".into(),
            value: Spanned::synthetic(var("r_within_arm")),
        }]),
        body: var("r_within_body"),
    });
    let scope_grant = Spanned::synthetic(CompKind::Grant {
        caps: vec![("net".into(), svar("r_grant_caps"))].into(),
        body: var("r_grant_body"),
    });
    let scope_audit = Spanned::synthetic(CompKind::Audit {
        body: var("r_audit_body"),
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
    let assemble_list = Spanned::synthetic(CompKind::Assemble(Assembly::List(vec![
        ValListElem::Single(svar("r_assemble_list_single")),
        ValListElem::Spread(svar("r_assemble_list_spread")),
    ])));
    let assemble_record = Spanned::synthetic(CompKind::Assemble(Assembly::Record(vec![
        ValRecordEntry::Field("lbl".into(), svar("r_assemble_record_value")),
        ValRecordEntry::Spread(svar("r_assemble_record_spread")),
    ])));
    let assemble_map = Spanned::synthetic(CompKind::Assemble(Assembly::Map(vec![
        ValMapEntry::Entry(var("r_assemble_map_key"), svar("r_assemble_map_value")),
        ValMapEntry::Spread(svar("r_assemble_map_spread")),
    ])));
    let val_variant = Spanned::synthetic(CompKind::Return(Val::Variant {
        label: "lbl".into(),
        payload: Some(Box::new(var("r_variant_payload"))),
    }));
    let val_variant_empty = Spanned::synthetic(CompKind::Return(Val::Variant {
        label: "lbl_empty".into(),
        payload: None,
    }));
    let val_thunk = Spanned::synthetic(CompKind::Return(Val::thunk(Arc::new(Spanned::synthetic(
        CompKind::Return(var("r_thunk_body")),
    )))));
    let capture = Spanned::synthetic(CompKind::Capture(ret("r_capture_body")));
    let decode = Spanned::synthetic(CompKind::Decode(var("r_decode_body")));

    // No single `CompKind` holds an arbitrary list of sub-`Comp`s to
    // wrap all of the above in one tree, so each is walked on its own
    // and the harvests are unioned.
    let nodes: Vec<Arc<Comp>> = vec![
        Arc::new(Spanned::synthetic(CompKind::Force(var("r_force")))),
        Arc::new(Spanned::synthetic(CompKind::Return(var("r_return")))),
        Arc::new(lam),
        Arc::new(bind),
        Arc::new(app),
        Arc::new(exec_name),
        Arc::new(exec_external),
        Arc::new(pipeline),
        Arc::new(binary),
        Arc::new(not),
        Arc::new(index),
        Arc::new(interpolation),
        Arc::new(rec),
        Arc::new(tilde),
        Arc::new(if_),
        Arc::new(case),
        Arc::new(scope_try),
        Arc::new(scope_guard),
        Arc::new(scope_within),
        Arc::new(scope_grant),
        Arc::new(scope_audit),
        Arc::new(scope_redirect),
        Arc::new(val_list),
        Arc::new(val_record),
        Arc::new(val_map),
        Arc::new(assemble_list),
        Arc::new(assemble_record),
        Arc::new(assemble_map),
        Arc::new(val_variant),
        Arc::new(val_variant_empty),
        Arc::new(val_thunk),
        Arc::new(capture),
        Arc::new(decode),
    ];

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
        "r_app_head",
        "r_app_arg_single",
        "r_app_arg_spread",
        "r_exec_name_head",
        "r_exec_arg",
        "r_exec_redirect_target",
        "r_exec_external_head",
        "r_pipeline_stage1",
        "r_pipeline_stage2",
        "r_binary_a",
        "r_binary_b",
        "r_not",
        "r_index_target",
        "r_index_key",
        "r_interp_a",
        "r_interp_b",
        "r_rec_member",
        "r_if_cond",
        "r_if_then",
        "r_if_else",
        "r_case_scrutinee",
        "r_case_arm_body",
        "r_try_body",
        "r_try_handler",
        "r_guard_body",
        "r_guard_cleanup",
        "r_within_opts",
        "r_within_arm",
        "r_within_body",
        "r_grant_caps",
        "r_grant_body",
        "r_audit_body",
        "r_scope_redirect_body",
        "r_scope_redirect_target",
        "r_list_single",
        "r_record_value",
        "r_map_value",
        "r_assemble_list_single",
        "r_assemble_list_spread",
        "r_assemble_record_value",
        "r_assemble_record_spread",
        "r_assemble_map_key",
        "r_assemble_map_value",
        "r_assemble_map_spread",
        "r_variant_payload",
        "r_thunk_body",
        "r_capture_body",
        "r_decode_body",
    ];

    for name in expected {
        assert!(found.contains(name), "missing reference: {name}");
    }
    assert_eq!(
        found.len(),
        expected.len(),
        "unexpected extra name in {found:?}; every bound-not-referenced \
         name (lam_param_bound, bind_map_bound, bind_rest_bound, \
         rec_name_bound) must be absent"
    );

    for bound in [
        "lam_param_bound",
        "bind_map_bound",
        "bind_rest_bound",
        "rec_name_bound",
    ] {
        assert!(
            !found.contains(bound),
            "a bound (not referenced) name leaked into the harvest: {bound}"
        );
    }
}

/// A thunk mentioning `b`, `a`, `b` and nesting a thunk over `c`, `a` has
/// occ `[a, b, c]`: sorted, distinct, and read off the inner node rather
/// than by re-walking its body.
#[test]
fn occ_is_sorted_distinct_and_nested_nodes_are_not_walked() {
    let inner = Val::thunk(Arc::new(Spanned::synthetic(CompKind::Binary(
        BinaryOp::Arith(ArithOp::Add),
        var("c"),
        var("a"),
    ))));
    let outer = ThunkNode::new(Arc::new(Spanned::synthetic(CompKind::Interpolation(vec![
        var("b"),
        var("a"),
        var("b"),
        inner,
    ]))));
    let occ = outer.occ();
    assert_eq!(occ.len(), 3, "sorted and distinct: a, b, c");
    for name in ["a", "b", "c"] {
        assert!(occ.contains(name), "occ missing {name}");
    }
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
