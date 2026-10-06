use super::*;
use crate::first_order::Finite;
use crate::path::tilde::TildePath;
use crate::syntax::parser::parse;

/// Head name, args, and whether the head was written `^name`.
fn expect_exec_name(comp: &Comp) -> (&CommandName, &Args, bool) {
    let CompKind::Exec(e) = &comp.item else {
        panic!("expected exec, got {:?}", comp.item);
    };
    let caret = matches!(e.head, CommandWord::External(_));
    (e.head.name(), &e.args, caret)
}

/// Strip spans so assertions match on shape alone.  Only lists and maps
/// hold one; a thunk's inner `Comp` spans are out of reach, so no fixture
/// here compares a thunk.
fn strip_slot(elem: &ValListElem) -> ValListElem {
    match elem {
        ValListElem::Single(v) => ValListElem::Single(Spanned::synthetic(strip_val(&v.item))),
        ValListElem::Spread(v) => ValListElem::Spread(Spanned::synthetic(strip_val(&v.item))),
    }
}

fn strip_val(val: &Val) -> Val {
    match val {
        Val::List(elems) => Val::list(
            elems
                .shape()
                .iter()
                .map(|v| Spanned::synthetic(strip_val(&v.item)))
                .collect::<Vec<_>>(),
        ),
        Val::Record(entries) => Val::record(
            entries
                .shape()
                .iter()
                .map(|(k, v)| (k.clone(), Spanned::synthetic(strip_val(&v.item))))
                .collect::<Vec<_>>(),
        ),
        Val::Map(entries) => Val::map(
            entries
                .shape()
                .iter()
                .map(|(k, v)| (k.clone(), Spanned::synthetic(strip_val(&v.item))))
                .collect::<Vec<_>>(),
        ),
        other => other.clone(),
    }
}

fn arg_items(args: &Args) -> Vec<ValListElem> {
    args.iter().map(strip_slot).collect()
}

/// Elaborate one statement, unwrapped from its sole `Run` phrase — for
/// tests over a single non-`let` statement.
fn elaborate_one(ast: &[Stmt], bindings: impl IntoIterator<Item = Name>, name: &str) -> Arc<Comp> {
    let top = elaborate(ast, bindings, name).expect("elaborate");
    let [phrase] = top.as_slice() else {
        panic!("expected one phrase, got {top:?}");
    };
    let Phrase::Run(comp) = &phrase.item else {
        panic!("expected a Run phrase, got {:?}", phrase.item);
    };
    comp.clone()
}

#[test]
fn tilde_path_command_head_elaborates_to_exec() {
    let ast = parse("~/.local/bin/claude update").expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let (name, args, _) = expect_exec_name(&comp);
    assert_eq!(
        name,
        &CommandName::TildePath(TildePath {
            suffix: Some("/.local/bin/claude".into()),
        })
    );
    assert_eq!(
        arg_items(args),
        vec![ValListElem::Single(Spanned::synthetic(Val::String(
            "update".into()
        )))]
    );
}

#[test]
fn tilde_path_command_head_without_args_elaborates_to_exec() {
    let ast = parse("~/.local/bin/claude").expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let (name, args, _) = expect_exec_name(&comp);
    assert_eq!(
        name,
        &CommandName::TildePath(TildePath {
            suffix: Some("/.local/bin/claude".into()),
        })
    );
    assert!(args.is_empty());
}

#[test]
fn literal_path_head_elaborates_to_direct_exec() {
    let ast = parse("./script").expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let (name, args, _) = expect_exec_name(&comp);
    assert_eq!(name, &CommandName::Path("./script".into()));
    assert!(args.is_empty());
}

#[test]
fn external_name_head_elaborates_to_external_exec() {
    let ast = parse("^git status").expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let (name, args, caret) = expect_exec_name(&comp);
    assert_eq!(name, &CommandName::Bare("git".into()));
    assert_eq!(
        arg_items(args),
        vec![ValListElem::Single(Spanned::synthetic(Val::String(
            "status".into()
        )))]
    );
    assert!(caret);
}

#[test]
fn explicit_value_head_elaborates_to_app() {
    // A value head takes no wrapping `Force`: `apply` forces a thunk in
    // head position at runtime, which leaves a `<file` redirect on the
    // `App` free to bracket the body.
    let ast = parse("$map $upper ['a']").expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let CompKind::App { head, args } = &comp.item else {
        panic!("expected app, got {:?}", comp.item);
    };
    let CompKind::Return(Val::Variable(name)) = &head.item else {
        panic!("expected returned-variable head, got {:?}", head.item);
    };
    assert_eq!(name.as_ref(), "map");
    assert_eq!(
        arg_items(args),
        vec![
            ValListElem::Single(Spanned::synthetic(Val::Variable("upper".into()))),
            ValListElem::Single(Spanned::synthetic(Val::list(vec![Spanned::synthetic(
                Val::String("a".into())
            )]))),
        ]
    );
}

/// Only thunk-form bindings are forward-declared — the shapes
/// `syntax::group` knots — so a command use preceding a non-thunk `let` on
/// the same name stays an `Exec`, not a `Force` of an unbound variable.
#[test]
fn command_use_before_non_thunk_let_is_exec() {
    let ast = parse("date\nlet date = 5").expect("parse");
    let top = elaborate(&ast, [], "").expect("elaborate");
    assert_eq!(top.len(), 2);
    let Phrase::Run(comp) = &top[0].item else {
        panic!("expected a Run phrase, got {:?}", top[0].item);
    };
    let (name, _, _) = expect_exec_name(comp);
    assert_eq!(name, &CommandName::Bare("date".into()));
}

/// An acyclic singleton `let f = { return 1 }` emits as a `Single`, whose
/// `Bind` runs after an earlier use of `f` — so that use must be an `Exec`.
#[test]
fn command_use_before_acyclic_thunk_let_is_exec() {
    let ast = parse("f\nlet f = { return 1 }").expect("parse");
    let top = elaborate(&ast, [], "").expect("elaborate");
    assert_eq!(top.len(), 2);
    let Phrase::Run(comp) = &top[0].item else {
        panic!("expected a Run phrase, got {:?}", top[0].item);
    };
    let (name, _, _) = expect_exec_name(comp);
    assert_eq!(name, &CommandName::Bare("f".into()));
}

/// A use of `g` ahead of its self-recursive definition still lowers to
/// `Exec`, while the self-reference inside the group resolves to the
/// forward-declared binding.
#[test]
fn command_use_before_recursive_thunk_let_is_exec() {
    let ast = parse("g 3\nlet g = { |n| g $[$n - 1] }").expect("parse");
    let top = elaborate(&ast, [], "").expect("elaborate");
    assert_eq!(top.len(), 2);
    let Phrase::Run(comp) = &top[0].item else {
        panic!("expected a Run phrase, got {:?}", top[0].item);
    };
    let (name, _, _) = expect_exec_name(comp);
    assert_eq!(name, &CommandName::Bare("g".into()));
    let Phrase::Define {
        pattern, comp: rhs, ..
    } = &top[1].item
    else {
        panic!("expected a self-recursive Define, got {:?}", top[1].item);
    };
    assert!(matches!(pattern.as_ref(), Pattern::Name(n) if n.as_ref() == "g"));
    let CompKind::Return(Val::Thunk(rec)) = &rhs.item else {
        panic!("expected Return(Thunk(Rec)), got {:?}", rhs.item);
    };
    assert!(
        matches!(rec.shape().item, CompKind::Rec { index: 0, .. }),
        "expected the self-recursive binding to emit a Rec{{index: 0}}, got {:?}",
        rec.shape().item
    );
}

/// The self-reference inside `f`'s body forces the forward-declared
/// variable rather than shelling out to a command named `f`.
#[test]
fn intra_group_recursion_resolves_to_binding() {
    let ast = parse("let f = { |n| f $n }\nf 5").expect("parse");
    let top = elaborate(&ast, [], "").expect("elaborate");
    assert_eq!(top.len(), 2);
    let Phrase::Define { comp: rhs, .. } = &top[0].item else {
        panic!("expected a Define, got {:?}", top[0].item);
    };
    let CompKind::Return(Val::Thunk(rec)) = &rhs.item else {
        panic!("expected Return(Thunk(Rec)), got {:?}", rhs.item);
    };
    let CompKind::Rec { group, index } = &rec.shape().item else {
        panic!("expected a Rec node, got {:?}", rec.shape().item);
    };
    let (_, member) = &group.shape()[*index];
    let CompKind::Lam { body, .. } = &member.item else {
        panic!("expected a lambda RHS, got {:?}", member.item);
    };
    assert!(
        matches!(body.item, CompKind::App { .. }),
        "expected the self-reference to force the bound variable, got {:?}",
        body.item
    );
}

/// A `?`-chain arm is guarded, so the interpolation's `!{…}` hoist must
/// live inside the arm: the statement elaborates to a bare `Try`, never
/// to a `Bind` that would run the hoist before the chain.
#[test]
fn chain_arm_hoist_stays_inside_the_arm() {
    let ast = parse(r#"return ok ? echo "fallback: !{hostname}""#).expect("parse");
    let comp = elaborate_one(&ast, [], "");
    assert!(
        matches!(comp.item, CompKind::Try { .. }),
        "chain arm hoist leaked into the caller: expected a bare Try, got {:?}",
        comp.item
    );
}

/// A sub-expression hoisted out of a command argument is emitted under
/// that argument's span, not the enclosing call's, so a runtime error in
/// it underlines the argument.
#[test]
fn a_hoisted_argument_keeps_its_own_span() {
    let src = "echo $xs[9]";
    let ast = parse(src).expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let CompKind::Bind { comp: rhs, .. } = &comp.item else {
        panic!(
            "expected the index to hoist into a Bind, got {:?}",
            comp.item
        );
    };
    let span = rhs.span.expect("a hoisted argument must carry a span");
    assert_eq!(
        &src[span.start as usize..span.end as usize],
        "$xs[9]",
        "the hoist carried the whole call's span"
    );
}

/// `$SCRIPT` resolves to a string literal, not a runtime lookup.
#[test]
fn script_bakes_to_a_string_literal() {
    let ast = parse("return $SCRIPT").expect("parse");
    let comp = elaborate_one(&ast, [], "/repo/lib.ral");
    assert_eq!(
        comp.item,
        CompKind::Return(Val::String("/repo/lib.ral".into()))
    );
}

/// Sources with no script identity — the REPL, `-c`, a preloaded `<...>`
/// source — reject `$SCRIPT` at elaboration time.
#[test]
fn script_with_no_identity_is_an_elaboration_error() {
    let ast = parse("return $SCRIPT").expect("parse");
    assert!(elaborate(&ast, [], "").is_err());
    assert!(elaborate(&ast, [], "-c").is_err());
    assert!(elaborate(&ast, [], "<stdin>").is_err());
}

// ── phrases and right-nested binders ─────────────────────────────────

#[test]
fn nested_sequence_is_wildcard_bind() {
    let stmts = parse("date\necho hi").expect("parse");
    let mut elaborator = Elaborator::new_with_bindings([], "");
    let seq = elaborator.stmts_nested(&stmts);
    let CompKind::Bind {
        pattern,
        comp,
        rest,
        ..
    } = &seq.item
    else {
        panic!("expected a Bind, got {:?}", seq.item);
    };
    assert!(matches!(pattern.as_ref(), Pattern::Wildcard));
    let (name, _, _) = expect_exec_name(comp);
    assert_eq!(name, &CommandName::Bare("date".into()));
    let (name, _, _) = expect_exec_name(rest);
    assert_eq!(name, &CommandName::Bare("echo".into()));
}

#[test]
fn script_cannot_be_bound_through_a_rest_pattern() {
    let ast = parse("let [a, ...SCRIPT] = [1, 2]").expect("parse");
    assert!(elaborate(&ast, [], "main.ral").is_err());
}

#[test]
fn script_cannot_be_bound_through_a_recursive_knot() {
    for src in [
        "let SCRIPT = { echo $SCRIPT }",
        "let f = { SCRIPT }\nlet SCRIPT = { f }",
    ] {
        let ast = parse(src).expect("parse");
        assert!(
            matches!(group_stmts(&ast)[..], [StmtGroup::LetRec(_)]),
            "{src}"
        );
        let err = elaborate(&ast, [], "main.ral").expect_err(src);
        let span = err.span.expect("the refusal carries the binder's span");
        let (start, end) = (span.start as usize, span.end as usize);
        assert_eq!(&src[start..end], "SCRIPT", "{src}");
        assert!(src[..start].ends_with("let "), "{src}");
    }
}

#[test]
fn word_val_classifies_canonical_numbers() {
    assert_eq!(word_val("5"), Val::Int(5));
    assert_eq!(word_val("42"), Val::Int(42));
    assert_eq!(word_val("0"), Val::Int(0));
    assert_eq!(
        word_val("2.5"),
        Val::Float(Finite::new(2.5).expect("finite"))
    );
    assert_eq!(word_val("true"), Val::Bool(true));
    assert_eq!(word_val("unit"), Val::String("unit".into()));
    assert_eq!(word_val("hello"), Val::String("hello".into()));
}

#[test]
fn nested_let_wildcard_binds_a_fresh_name() {
    let stmts = parse("let _ = date\necho hi").expect("parse");
    let mut elaborator = Elaborator::new_with_bindings([], "");
    let seq = elaborator.stmts_nested(&stmts);
    let CompKind::Bind { pattern, .. } = &seq.item else {
        panic!("expected a Bind, got {:?}", seq.item);
    };
    match pattern.as_ref() {
        Pattern::Name(n) => assert!(crate::ir::is_synthetic(n), "not synthetic: {n}"),
        other => panic!("expected a fresh Name, never Wildcard, got {other:?}"),
    }
}

#[test]
fn nested_let_right_nests_over_following_statements() {
    let stmts = parse("let x = 1\necho hi\necho bye").expect("parse");
    let mut elaborator = Elaborator::new_with_bindings([], "");
    let seq = elaborator.stmts_nested(&stmts);
    let CompKind::Bind { pattern, rest, .. } = &seq.item else {
        panic!("expected a Bind, got {:?}", seq.item);
    };
    assert!(matches!(pattern.as_ref(), Pattern::Name(n) if n.as_ref() == "x"));
    let CompKind::Bind {
        pattern: inner_pattern,
        rest: inner_rest,
        ..
    } = &rest.item
    else {
        panic!(
            "expected the let to right-nest over echo hi, got {:?}",
            rest.item
        );
    };
    assert!(matches!(inner_pattern.as_ref(), Pattern::Wildcard));
    let (name, _, _) = expect_exec_name(inner_rest);
    assert_eq!(name, &CommandName::Bare("echo".into()));
}

#[test]
fn nested_block_ending_in_let_has_unit_tail() {
    let stmts = parse("let x = 1").expect("parse");
    let mut elaborator = Elaborator::new_with_bindings([], "");
    let seq = elaborator.stmts_nested(&stmts);
    let CompKind::Bind { rest, .. } = &seq.item else {
        panic!("expected a Bind, got {:?}", seq.item);
    };
    assert_eq!(rest.item, CompKind::Return(Val::Unit));
}

#[test]
fn toplevel_phrases_classify_define_and_run() {
    let ast = parse("let x = 1\necho hi").expect("parse");
    let top = elaborate(&ast, [], "").expect("elaborate");
    assert_eq!(top.len(), 2);
    assert!(matches!(top[0].item, Phrase::Define { .. }));
    assert!(matches!(top[1].item, Phrase::Run(_)));
}

#[test]
fn toplevel_self_recursive_let_is_a_rec_define() {
    let ast = parse("let f = { |n| f $n }").expect("parse");
    let top = elaborate(&ast, [], "").expect("elaborate");
    assert_eq!(top.len(), 1);
    let Phrase::Define { pattern, comp, .. } = &top[0].item else {
        panic!("expected a Define, got {:?}", top[0].item);
    };
    assert!(matches!(pattern.as_ref(), Pattern::Name(n) if n.as_ref() == "f"));
    let CompKind::Return(Val::Thunk(rec)) = &comp.item else {
        panic!("expected Return(Thunk(Rec)), got {:?}", comp.item);
    };
    let CompKind::Rec { group, index } = &rec.shape().item else {
        panic!("expected a Rec node, got {:?}", rec.shape().item);
    };
    assert_eq!(*index, 0);
    assert_eq!(
        group.shape().len(),
        1,
        "a self-recursive let is a group of one"
    );
}

#[test]
fn toplevel_mutual_recursion_shares_one_group_arc() {
    let ast = parse("let f = { |x| g $x }\nlet g = { |y| f $y }").expect("parse");
    let top = elaborate(&ast, [], "").expect("elaborate");
    assert_eq!(top.len(), 2);
    fn group_arc(phrase: &Phrase<()>) -> Arc<GroupNode> {
        let Phrase::Define { comp, .. } = phrase else {
            panic!("expected a Define, got {phrase:?}");
        };
        let CompKind::Return(Val::Thunk(rec)) = &comp.item else {
            panic!("expected Return(Thunk(Rec)), got {:?}", comp.item);
        };
        let CompKind::Rec { group, .. } = &rec.shape().item else {
            panic!("expected a Rec node, got {:?}", rec.shape().item);
        };
        Arc::clone(group)
    }
    let g1 = group_arc(&top[0].item);
    let g2 = group_arc(&top[1].item);
    assert!(
        Arc::ptr_eq(&g1, &g2),
        "both binders must share one group Arc"
    );
    assert_eq!(g1.shape()[0].0.as_ref(), "f");
    assert_eq!(g1.shape()[1].0.as_ref(), "g");
}

fn is_home_tilde(comp: &Comp) -> bool {
    comp.item == CompKind::Tilde(TildePath { suffix: None })
}

/// The hoisted read and what follows it, from `comp`'s outermost bind.
fn hoisted(comp: &Comp) -> (&Comp, &Pattern, &Comp) {
    let CompKind::Bind {
        comp: rhs,
        pattern,
        rest,
    } = &comp.item
    else {
        panic!("expected a Bind over a hoisted read, got {:?}", comp.item);
    };
    (rhs, pattern, rest)
}

#[test]
fn tilde_path_hoists_one_tilde() {
    let ast = parse("echo ~/x").expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let (rhs, _, rest) = hoisted(&comp);
    assert_eq!(
        rhs.item,
        CompKind::Tilde(TildePath {
            suffix: Some("/x".into())
        })
    );
    assert!(matches!(rest.item, CompKind::Exec(_)));
}

#[test]
fn leading_tilde_in_a_string_interpolates_the_home_tilde() {
    let ast = parse(r#""~/x""#).expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let (rhs, pattern, rest) = hoisted(&comp);
    assert!(is_home_tilde(rhs), "got {:?}", rhs.item);
    let Pattern::Name(tmp) = pattern else {
        panic!("expected a named temporary, got {pattern:?}");
    };
    let CompKind::Interpolation(parts) = &rest.item else {
        panic!("expected an Interpolation, got {:?}", rest.item);
    };
    assert_eq!(
        parts.as_slice(),
        [Val::Variable(tmp.clone()), Val::String("/x".into())]
    );
}

#[test]
fn let_of_lone_tilde_reads_rather_than_runs() {
    let ast = parse("let h = ~").expect("parse");
    let top = elaborate(&ast, [], "").expect("elaborate");
    let Phrase::Define { comp, .. } = &top[0].item else {
        panic!("expected a Define, got {:?}", top[0].item);
    };
    let (rhs, _, rest) = hoisted(comp);
    assert!(is_home_tilde(rhs), "got {:?}", rhs.item);
    assert!(matches!(rest.item, CompKind::Return(Val::Variable(_))));
}

/// A literal with no spread stays value syntax; one with a spread is a
/// computation, hoisted like any other under `to_val`.
#[test]
fn a_spread_literal_is_an_assembly_and_a_plain_one_a_value() {
    let ast = parse("return [1, $x]").expect("parse");
    let comp = elaborate_one(&ast, [], "");
    assert!(
        matches!(&comp.item, CompKind::Return(Val::List(_))),
        "expected Return(Val::List(_)), got {:?}",
        comp.item
    );

    let ast = parse("return [1, ...$xs]").expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let CompKind::Bind {
        comp: rhs, rest, ..
    } = &comp.item
    else {
        panic!(
            "expected a Bind over the hoisted assembly, got {:?}",
            comp.item
        );
    };
    assert!(
        matches!(rhs.item, CompKind::Assemble(Assembly::List(_))),
        "expected Assemble(List(_)), got {:?}",
        rhs.item
    );
    assert!(
        matches!(&rest.item, CompKind::Return(Val::Variable(_))),
        "expected the rest to return the hoisted temporary, got {:?}",
        rest.item
    );
}

/// An arm is a literal block or a name holding one: anything else would be
/// hoisted, and so run before the form chose.
#[test]
fn an_arm_that_would_hoist_is_refused() {
    for src in [
        "if true \"a$[1 + 1]\" else { return () }",
        "case `a () [`a: !{mk}]",
    ] {
        let ast = parse(src).expect("parse");
        let err = elaborate(&ast, [], "").expect_err(src);
        assert!(
            err.message.contains("an arm is a block"),
            "{src}: {}",
            err.message
        );
    }
    let ast = parse("let h = { return 1 }; if true $h else { return 2 }").expect("parse");
    assert!(
        elaborate(&ast, [], "").is_ok(),
        "a name holding a block is an arm"
    );
}

/// A literal arm is a thunk, forced by the form that took it; an `else`-less
/// `if` discards its lone arm's result.
#[test]
fn arms_are_thunks_and_a_lone_arm_returns_unit() {
    let ast = parse("if true { echo hi }").expect("parse");
    let comp = elaborate_one(&ast, [], "");
    let CompKind::If { then, else_, .. } = &comp.item else {
        panic!("expected an if, got {:?}", comp.item);
    };
    for arm in [then, else_] {
        assert!(matches!(arm.item, Val::Thunk(_)), "arm {:?}", arm.item);
    }
    let Val::Thunk(node) = &then.item else {
        unreachable!()
    };
    assert!(
        matches!(&node.shape().item, CompKind::Bind { pattern, .. } if matches!(**pattern, Pattern::Wildcard)),
        "the lone arm is wrapped to discard its result, got {:?}",
        node.shape().item
    );
}
