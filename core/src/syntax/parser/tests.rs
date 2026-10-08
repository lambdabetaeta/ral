use super::*;
use crate::ir::{CompareOp, EqOp, StderrTarget};
use crate::path::tilde::TildePath;
use crate::syntax::ast::ScopeAst;

fn plain(s: &str) -> Ast {
    Ast::Word(Word::Plain(s.into()))
}

/// Span-free `Spanned`.  These fixtures compare shapes, never positions,
/// so both sides of every assertion are normalised to `None` spans.
fn sp(a: Ast) -> Spanned<Ast> {
    Spanned::synthetic(a)
}

fn tilde_word(path: TildePath) -> Ast {
    Ast::Word(Word::Tilde(path))
}

fn bare_head(s: &str) -> Head {
    Head::Bare(s.into())
}

fn path_head(s: &str) -> Head {
    Head::Path(s.into())
}

fn external_head(s: &str) -> Head {
    Head::ExternalName(s.into())
}

fn value_head(ast: Ast) -> Head {
    Head::Value(Box::new(ast))
}

fn app(head: Head, args: Vec<Ast>) -> Ast {
    Ast::Call {
        head,
        args: args.into_iter().map(Spanned::synthetic).collect(),
    }
}

fn redirected(stage: Ast, redirects: Box<Redirects<Ast>>) -> Ast {
    Ast::Redirected {
        stage: Spanned::synthetic_boxed(stage),
        redirects,
    }
}

fn app_redir(head: Head, args: Vec<Ast>, redirects: Box<Redirects<Ast>>) -> Ast {
    redirected(app(head, args), redirects)
}

/// The stage and redirects of a parsed `Ast::Redirected`.
fn unwrap_redirected(ast: &Ast) -> (&Ast, &Redirects<Ast>) {
    match ast {
        Ast::Redirected { stage, redirects } => (&stage.item, redirects),
        other => panic!("expected a redirected stage, got {other:?}"),
    }
}

/// The redirects of `src`, the one statement it parses to.
fn redirects_of(src: &str) -> Redirects<Ast> {
    unwrap_redirected(&sole_stmt(src)).1.clone()
}

/// Bare `Ast`s in the `Vec<Stmt>` shape a block, lambda, or pipeline body
/// demands.
fn body(asts: Vec<Ast>) -> Vec<Stmt> {
    asts.into_iter().map(Spanned::synthetic).collect()
}

/// A parsed program as the bare `Vec<Ast>` the fixtures are written in.
fn unwrap_stmts(stmts: Vec<Stmt>) -> Vec<Ast> {
    stmts.into_iter().map(|s| strip_one(s.item)).collect()
}

/// Like [`unwrap_stmts`], but for a nested body, where the `Stmt` wrapper
/// has to stay for the expected shape to typecheck.
fn strip_stmts(stmts: Vec<Stmt>) -> Vec<Stmt> {
    stmts
        .into_iter()
        .map(|s| Spanned::synthetic(strip_one(s.item)))
        .collect()
}

fn strip_args(args: Vec<Ast>) -> Vec<Ast> {
    args.into_iter().map(strip_one).collect()
}

fn strip_spanned_args(args: Vec<Spanned<Ast>>) -> Vec<Spanned<Ast>> {
    args.into_iter()
        .map(|sp| Spanned::synthetic(strip_one(sp.item)))
        .collect()
}

fn strip_head(head: Head) -> Head {
    match head {
        Head::Value(ast) => Head::Value(Box::new(strip_one(*ast))),
        other => other,
    }
}

/// Drop every span and unwrap a lone head out of its `Call`, so a fixture
/// can name the shape without predicting byte positions.
fn strip_one(n: Ast) -> Ast {
    match n {
        Ast::Call { head, args } if args.is_empty() => match head {
            Head::Bare(s) => plain(&s),
            Head::Path(s) => Ast::Word(Word::Slash(s)),
            Head::TildePath(path) => tilde_word(path),
            Head::Value(ast) => strip_one(*ast),
            Head::ExternalName(s) => app(Head::ExternalName(s), vec![]),
        },
        Ast::Return(None) => Ast::Return(None),
        Ast::Return(Some(value)) => {
            Ast::Return(Some(Spanned::synthetic_boxed(strip_one(*value.item))))
        }
        Ast::Index { target, keys } => Ast::Index {
            target: Spanned::synthetic_boxed(strip_one(*target.item)),
            keys: strip_spanned_args(keys),
        },
        Ast::Call { head, args } => {
            let plain_args: Vec<Ast> = args.into_iter().map(|sp| sp.item).collect();
            app(strip_head(head), strip_args(plain_args))
        }
        Ast::Redirected { stage, redirects } => match *stage.item {
            Ast::Call { head, args } => {
                let plain_args: Vec<Ast> = args.into_iter().map(|sp| sp.item).collect();
                app_redir(strip_head(head), strip_args(plain_args), redirects)
            }
            other => redirected(strip_one(other), redirects),
        },
        Ast::Scope(op) => Ast::Scope(strip_scope(op)),
        Ast::Block(body) => Ast::Block(strip_stmts(body)),
        Ast::Lambda { param, body } => Ast::Lambda {
            param: Spanned::synthetic(param.item),
            body: strip_stmts(body),
        },
        Ast::Pipeline(stages) => Ast::Pipeline(strip_stmts(stages)),
        Ast::Chain(parts) => Ast::Chain(strip_spanned_args(parts)),
        Ast::Interpolation(parts) => Ast::Interpolation(strip_spanned_args(parts)),
        Ast::List(elems) => Ast::List(
            elems
                .into_iter()
                .map(|e| match e {
                    ListElem::Single(a) => ListElem::Single(Spanned::synthetic(strip_one(a.item))),
                    ListElem::Spread(a) => ListElem::Spread(Spanned::synthetic(strip_one(a.item))),
                })
                .collect(),
        ),
        Ast::Record(entries) => Ast::Record(
            entries
                .into_iter()
                .map(|e| match e {
                    RecordEntry::Field { key, value } => RecordEntry::Field {
                        key: Spanned::synthetic(key.item),
                        value: Spanned::synthetic(strip_one(value.item)),
                    },
                    RecordEntry::Spread(a) => {
                        RecordEntry::Spread(Spanned::synthetic(strip_one(a.item)))
                    }
                })
                .collect(),
        ),
        Ast::Map(entries) => Ast::Map(
            entries
                .into_iter()
                .map(|e| match e {
                    MapEntry::Entry { key, value } => MapEntry::Entry {
                        key: Spanned::synthetic(strip_one(key.item)),
                        value: Spanned::synthetic(strip_one(value.item)),
                    },
                    MapEntry::Spread(a) => MapEntry::Spread(Spanned::synthetic(strip_one(a.item))),
                })
                .collect(),
        ),
        Ast::Force(value) => Ast::Force(Spanned::synthetic_boxed(strip_one(*value.item))),
        Ast::Let { pattern, value } => Ast::Let {
            pattern: Spanned::synthetic(pattern.item),
            value: Spanned::synthetic_boxed(strip_one(*value.item)),
        },
        Ast::If { branches, else_ } => Ast::If {
            branches: branches
                .into_iter()
                .map(|b| IfBranch {
                    cond: Spanned::synthetic_boxed(strip_one(*b.cond.item)),
                    body: Spanned::synthetic_boxed(strip_one(*b.body.item)),
                })
                .collect(),
            else_: else_.map(|e| Spanned::synthetic_boxed(strip_one(*e.item))),
        },
        Ast::Case { scrutinee, arms } => Ast::Case {
            scrutinee: Spanned::synthetic_boxed(strip_one(*scrutinee.item)),
            arms: arms
                .into_iter()
                .map(|arm| CaseArm {
                    tag: Spanned::synthetic(arm.tag.item),
                    body: Spanned::synthetic_boxed(strip_one(*arm.body.item)),
                })
                .collect(),
        },
        Ast::Tag { label, payload } => Ast::Tag {
            label,
            payload: payload.map(|p| Spanned::synthetic_boxed(strip_one(*p.item))),
        },
        Ast::Spread(value) => Ast::Spread(Spanned::synthetic_boxed(strip_one(*value.item))),
        Ast::Binary(l, op, r) => Ast::Binary(strip_boxed(l), op, strip_boxed(r)),
        Ast::And(l, r) => Ast::And(strip_boxed(l), strip_boxed(r)),
        Ast::Or(l, r) => Ast::Or(strip_boxed(l), strip_boxed(r)),
        Ast::Negate(inner) => Ast::Negate(strip_boxed(inner)),
        Ast::Not(inner) => Ast::Not(strip_boxed(inner)),
        other => other,
    }
}

fn strip_boxed(node: Spanned<Box<Ast>>) -> Spanned<Box<Ast>> {
    Spanned::synthetic_boxed(strip_one(*node.item))
}

fn strip_opts(opts: Options) -> Options {
    opts.into_iter()
        .map(|(name, value)| (name, Spanned::synthetic(strip_one(value.item))))
        .collect()
}

fn strip_scope(op: ScopeAst) -> ScopeAst {
    let s = |a: Box<Ast>| Box::new(strip_one(*a));
    match op {
        ScopeAst::Try { body, handler } => ScopeAst::Try {
            body: s(body),
            handler: s(handler),
        },
        ScopeAst::Guard { body, cleanup } => ScopeAst::Guard {
            body: s(body),
            cleanup: s(cleanup),
        },
        ScopeAst::Within {
            opts,
            handlers,
            body,
        } => ScopeAst::Within {
            opts: strip_opts(opts),
            handlers: handlers.map(|arms| {
                arms.into_iter()
                    .map(|arm| HandlerArm {
                        name: arm.name,
                        value: Spanned::synthetic(strip_one(arm.value.item)),
                    })
                    .collect()
            }),
            body: s(body),
        },
        ScopeAst::Grant { caps, body } => ScopeAst::Grant {
            caps: strip_opts(caps),
            body: s(body),
        },
        ScopeAst::Audit { body } => ScopeAst::Audit { body: s(body) },
    }
}

#[test]
fn parse_simple_command() {
    let ast = unwrap_stmts(parse("echo hello").unwrap());
    assert_eq!(ast, vec![app(bare_head("echo"), vec![plain("hello")])]);
}

#[test]
fn parse_variable() {
    let ast = unwrap_stmts(parse("echo $x").unwrap());
    assert_eq!(
        ast,
        vec![app(bare_head("echo"), vec![Ast::Variable("x".into())])]
    );
}

#[test]
fn parse_explicit_value_head_application() {
    let ast = unwrap_stmts(parse("$map $upper ['a']").unwrap());
    assert_eq!(
        ast,
        vec![app(
            value_head(Ast::Variable("map".into())),
            vec![
                Ast::Variable("upper".into()),
                Ast::List(vec![ListElem::Single(sp(Ast::Literal("a".into())))]),
            ],
        )]
    );
}

#[test]
fn parse_explicit_value_head_without_args_remains_value() {
    let ast = unwrap_stmts(parse("$map").unwrap());
    assert_eq!(ast, vec![Ast::Variable("map".into())]);
}

#[test]
fn parse_external_name_head_application() {
    let ast = unwrap_stmts(parse("^git status").unwrap());
    assert_eq!(ast, vec![app(external_head("git"), vec![plain("status")])]);
}

#[test]
fn parse_external_name_head_without_args() {
    let ast = parse("^git").unwrap();
    match ast.as_slice() {
        [
            Stmt {
                item: Ast::Call { head, args, .. },
                ..
            },
        ] => {
            assert_eq!(args.as_slice(), []);
            assert_eq!(head, &external_head("git"));
        }
        _ => panic!("expected zero-arg external-name app, got {ast:?}"),
    }
}

#[test]
fn parse_external_name_rejected_in_arg_position() {
    assert!(parse("echo ^git").is_err());
}

#[test]
fn parse_binding() {
    let ast = unwrap_stmts(parse("let x = hello").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::Name("x".into())),
            value: Spanned::synthetic_boxed(plain("hello")),
        }]
    );
}

#[test]
fn parse_pipeline() {
    let ast = unwrap_stmts(parse("echo hello | upper").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Pipeline(body(vec![
            app(bare_head("echo"), vec![plain("hello")]),
            plain("upper"),
        ]))]
    );
}

#[test]
fn parse_pipeline_quoted_literal_stage() {
    let ast = unwrap_stmts(parse("'abc' | blah").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Pipeline(body(vec![
            Ast::Literal("abc".into()),
            plain("blah"),
        ]))]
    );
}

#[test]
fn parse_chain() {
    let ast = unwrap_stmts(parse("return true ? echo yes").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Chain(vec![
            sp(Ast::Return(Some(Spanned::synthetic_boxed(plain("true"))))),
            sp(app(bare_head("echo"), vec![plain("yes")])),
        ])]
    );
}

#[test]
fn parse_let_rhs_chain() {
    // The whole chain binds to `x`, not just `a`.
    let ast = unwrap_stmts(parse("let x = a ? b").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::Name("x".into())),
            value: Spanned::synthetic_boxed(Ast::Chain(vec![sp(plain("a")), sp(plain("b")),])),
        }]
    );
}

/// The bash-backgrounding reflex earns an error naming `spawn`, in every
/// position trailing `&` can appear: statement, chain arm, pipeline tail,
/// `let` RHS.
#[test]
fn trailing_amp_is_rejected_for_spawn() {
    for src in [
        "sleep 10 &",
        "a ? b &",
        "cat log | grep error &",
        "let x = cargo build &",
        "{ sleep 10 & }",
    ] {
        let err = parse(src).expect_err("`&` must not parse");
        assert!(
            err.message.contains("does not background") && err.message.contains("spawn"),
            "expected the spawn correspondence for {src:?}, got: {}",
            err.message
        );
    }
}

#[test]
fn parse_let_after_pipe_rejected() {
    let err = parse("cmd | let x = y").unwrap_err();
    assert!(
        err.message.contains("`let`"),
        "expected let-placement error, got: {}",
        err.message
    );
}

#[test]
fn parse_let_after_question_rejected() {
    let err = parse("cmd ? let x = y").unwrap_err();
    assert!(
        err.message.contains("`let`"),
        "expected let-placement error, got: {}",
        err.message
    );
}

// ── Sub-token-stream parsers require EOF ─────────────────────────

/// Inside `$[…]` a `>` is a comparison however it is spaced: `2>` is not
/// a file descriptor there.
#[test]
fn glued_comparison_is_a_comparison() {
    for src in ["return $[2>3]", "return $[2 > 3]", "return $[$x<3]"] {
        let ast = unwrap_stmts(parse(src).unwrap());
        let Ast::Return(Some(v)) = &ast[0] else {
            panic!("{src:?}: expected a return, got {ast:?}");
        };
        assert!(
            matches!(
                *v.item,
                Ast::Binary(_, BinaryOp::Compare(CompareOp::Gt | CompareOp::Lt), _)
            ),
            "{src:?}: expected a comparison, got {:?}",
            v.item
        );
    }

    let err = parse("return $[< 3]").unwrap_err();
    assert!(
        err.message.contains("operand on each side"),
        "expected an operator-position error, got: {}",
        err.message
    );
}

/// An operand is any atom, so a string may be compared — the typechecker,
/// not the grammar, says what `+` accepts.
#[test]
fn expression_operands_are_atoms() {
    let ast = unwrap_stmts(parse("$[$s == 'quit']").unwrap());
    assert_eq!(
        ast,
        vec![binary(
            Ast::Variable("s".into()),
            BinaryOp::Eq(EqOp::Eq),
            Ast::Literal("quit".into()),
        )]
    );
    let ast = unwrap_stmts(parse("$[!{f}[k] + [1][0]]").unwrap());
    assert!(matches!(
        ast[0],
        Ast::Binary(_, BinaryOp::Arith(ArithOp::Add), _)
    ));
}

/// Two operands with no operator between them name the gap, not
/// "trailing input".
#[test]
fn juxtaposed_operands_name_the_missing_operator() {
    let err = parse("$[1 2]").unwrap_err();
    assert!(
        err.message.contains("expected an operator"),
        "got: {}",
        err.message
    );
}

/// `=` is the one spelling the lexer emits inside `$[…]` that is no operator:
/// equality is `==`, and binding is `let`.
#[test]
fn a_lone_equals_in_an_expression_names_the_equality_operator() {
    for src in ["$[1 = 1]", "$[$x = 2 && true]", "$[= 1]"] {
        let err = parse(src).unwrap_err();
        assert!(
            err.message.contains("`=` is not an operator") && err.message.contains("`==`"),
            "{src:?}: {}",
            err.message
        );
    }
}

/// Outside `$[…]` the shell meaning of `>` stands, so `>=` is a redirect
/// to a file named `=…`, exactly as §3.5 lists `>` among the word-enders.
#[test]
fn comparison_spellings_are_redirects_outside_expressions() {
    assert!(redirects_of("echo a >= b").stdout.is_some());
}

/// The bash reflexes `&&` and `||` each earn an error naming ral's own
/// spelling, rather than a stray-token complaint.
#[test]
fn logical_connectives_outside_expressions_are_refused_by_name() {
    let err = parse("echo a && echo b").unwrap_err();
    assert!(err.message.contains("no `&&`"), "got: {}", err.message);
    let err = parse("echo a || echo b").unwrap_err();
    assert!(err.message.contains("no `||`"), "got: {}", err.message);
}

/// Outside `$[…]` parentheses only spell unit, so a reader reaching for
/// grouping is sent to the forms that group rather than told a token was
/// unexpected.
#[test]
fn a_lone_paren_names_the_unit_literal_and_the_expression_form() {
    let err = parse("echo (1)").unwrap_err();
    assert!(
        err.message.contains("`()`") && err.message.contains("$[…]"),
        "expected the unit literal and `$[…]` both named, got: {}",
        err.message
    );
}

#[test]
fn touching_atoms_in_command_position_are_refused() {
    for src in [
        "echo --prefix=$d",
        "echo --prefix='/opt/my dir'",
        "echo $p/x",
        "echo $n.txt",
        "echo 'a'\"b\"'c'",
        "echo $h[a]x",
        "echo'x'",
        "echo a > $dir/out",
        "echo a$[1+1]b",
    ] {
        let err = parse(src).unwrap_err();
        assert!(
            matches!(err.kind, ParseErrorKind::Touching { .. }),
            "{src:?} must be refused as touching, got: {}",
            err.message
        );
    }
}

/// The words rule is checked once, where a unit ends: whatever begins there
/// is the second word, and the run is everything that keeps touching.
#[test]
fn touching_is_refused_naming_both_words_and_the_run() {
    for (src, first, second, run) in [
        ("try{a}{b}", "try", "{a}", "try{a}{b}"),
        ("try{a} {b}", "try", "{a}", "try{a}"),
        ("if $c{a}", "$c", "{a}", "$c{a}"),
        ("return'x'", "return", "'x'", "return'x'"),
        ("[a'b']", "a", "'b'", "a'b'"),
        ("$[1'a']", "1", "'a'", "1'a'"),
        ("$[(1)'a']", "(1)", "'a'", "(1)'a'"),
        ("$(red)[x]", "$(red)", "[x]", "$(red)[x]"),
        ("cmd >$x'a'", "$x", "'a'", "$x'a'"),
        ("{a}'b'", "{a}", "'b'", "{a}'b'"),
        ("$[not$x]", "not", "$x", "not$x"),
        ("^ls$x", "^ls", "$x", "^ls$x"),
    ] {
        let err = parse(src).unwrap_err();
        let ParseErrorKind::Touching {
            first: f,
            second: s,
            run: r,
        } = err.kind
        else {
            panic!("{src:?} must be refused as touching, got: {}", err.message);
        };
        let text = |span: Span| &src[span.range()];
        assert_eq!((text(f), text(s), text(r)), (first, second, run), "{src:?}");
    }
}

#[test]
fn grammatical_adjacency_still_parses() {
    for src in [
        "echo $h[k]",
        "echo !{f}[k]",
        "echo ...$xs",
        "echo a >file",
        "echo a 2>x",
        "echo a|cat",
        "echo a;echo b",
        "{ echo a }",
        "echo a ? echo b",
        "echo [1, 2]",
        "echo a 'b' \"c\" $d",
        "a>f",
        "cmd >file",
        "cmd a> f",
        "$[1+1]",
        "$[-1]",
        "$[1+-1]",
        "$x[0]",
        "[1][2]",
        "!{f}[k]",
        "`ok 5",
    ] {
        if let Err(err) = parse(src) {
            panic!("{src:?} must parse, got: {}", err.message);
        }
    }
}

/// `^`, `...` and `!` mark what follows them, so each must touch it.
#[test]
fn an_attachment_must_touch_what_it_marks() {
    for (src, said) in [
        ("^ ls", "`^` attaches to the name it marks: write `^ls`"),
        (
            "echo ... $x",
            "`...` attaches to the value it spreads: write `...$xs`",
        ),
        ("echo [... $x]", "`...` attaches to the value it spreads"),
        (
            "let [a, ... r] = $x",
            "`...` attaches to the value it spreads",
        ),
        (
            "echo ! $x",
            "`!` attaches to what it forces: write `!{…}` or `!$x`",
        ),
    ] {
        let err = parse(src).unwrap_err();
        assert!(
            err.message.contains(said),
            "{src:?} must say {said:?}, got: {}",
            err.message
        );
    }
}

/// With statements after the stray brace, "trailing input" would be doubly
/// wrong: it is not trailing, and the parse has not completed.
#[test]
fn mid_program_stray_rbrace_names_unmatched_brace() {
    let err = parse("{ let x = 1 } } let y = 2").unwrap_err();
    assert!(
        err.message.contains("unmatched `}`"),
        "expected an unmatched-brace error, got: {}",
        err.message
    );
}

#[test]
fn well_formed_block_still_parses() {
    let ast = unwrap_stmts(parse("echo a; { echo b }").unwrap());
    assert_eq!(
        ast,
        vec![
            app(bare_head("echo"), vec![plain("a")]),
            Ast::Block(body(vec![app(bare_head("echo"), vec![plain("b")])])),
        ]
    );
}

#[test]
fn parse_chain_continues_across_newline_before_question() {
    let ast = unwrap_stmts(parse("a\n? b").unwrap());
    assert_eq!(ast, vec![Ast::Chain(vec![sp(plain("a")), sp(plain("b"))])]);
}

/// A trailing `?` continues the chain, as a trailing `|` does — which is
/// what the REPL's continuation prompt already promises.
#[test]
fn parse_chain_continues_across_newline_after_question() {
    for src in ["a ?\nb", "a ?\n\n  b", "a\n?\nb"] {
        assert_eq!(
            unwrap_stmts(parse(src).unwrap()),
            vec![Ast::Chain(vec![sp(plain("a")), sp(plain("b"))])],
            "{src:?}"
        );
    }
}

#[test]
fn parse_semicolon_never_continues() {
    for src in [
        "a ; | b", "a | ; b", "a ;\n| b", "a |\n; b", "a ; ? b", "a ? ; b", "a\n; ? b",
    ] {
        assert!(parse(src).is_err(), "{src:?} must not parse");
    }
}

#[test]
fn parse_semicolon_separates_statements() {
    let ast = unwrap_stmts(parse("a ; b").unwrap());
    assert_eq!(ast, vec![plain("a"), plain("b")]);
}

#[test]
fn parse_lambda_arg() {
    let ast = unwrap_stmts(parse("echo { |x| echo $x }").unwrap());
    assert_eq!(
        ast,
        vec![app(
            bare_head("echo"),
            vec![Ast::Lambda {
                param: Spanned::synthetic(Pattern::Name("x".into())),
                body: body(vec![app(
                    bare_head("echo"),
                    vec![Ast::Variable("x".into())]
                )]),
            }],
        )]
    );
}

#[test]
fn parse_return_stage() {
    let ast = unwrap_stmts(parse("return $x").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::Variable(
            "x".into()
        ),)))]
    );
}

#[test]
fn the_empty_string_is_a_literal() {
    assert_eq!(
        unwrap_stmts(parse("return \"\"").unwrap()),
        vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::Literal(
            String::new()
        ))))]
    );
    assert_eq!(
        unwrap_stmts(parse("echo \"\"").unwrap()),
        vec![app(bare_head("echo"), vec![Ast::Literal(String::new())])]
    );
}

#[test]
fn parse_return_unit_stage() {
    let ast = unwrap_stmts(parse("return").unwrap());
    assert_eq!(ast, vec![Ast::Return(None)]);
}

#[test]
fn parse_return_force_argument() {
    let ast = unwrap_stmts(parse("return !{hostname}").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::Force(
            Spanned::synthetic_boxed(Ast::Block(body(vec![plain("hostname"),])))
        ),)))]
    );
}

#[test]
fn parse_list() {
    let ast = unwrap_stmts(parse("return [a, b, c]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::List(
            vec![
                ListElem::Single(sp(plain("a"))),
                ListElem::Single(sp(plain("b"))),
                ListElem::Single(sp(plain("c"))),
            ]
        ),)))]
    );
}

#[test]
fn parse_record() {
    let ast = unwrap_stmts(parse("return [host: localhost, port: 8080]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::Record(
            vec![
                RecordEntry::Field {
                    key: Spanned::synthetic("host".into()),
                    value: sp(plain("localhost")),
                },
                RecordEntry::Field {
                    key: Spanned::synthetic("port".into()),
                    value: sp(plain("8080")),
                },
            ]
        ),)))]
    );
}

/// A `"quoted"` key is data, so one makes the literal a map, and a value of
/// another type under a second key is the map element rule's business, not
/// the parser's.
#[test]
fn parse_quoted_key_makes_a_map() {
    let ast = unwrap_stmts(parse("[\"a\": 1, 'b': 2]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Map(vec![
            MapEntry::Entry {
                key: sp(Ast::Literal("a".into())),
                value: sp(plain("1")),
            },
            MapEntry::Entry {
                key: sp(Ast::Literal("b".into())),
                value: sp(plain("2")),
            },
        ])]
    );
}

/// `[:]` is the one literal that opens with `:`; the old `[:, …]` marker is
/// refused with the quoted-key spelling.
#[test]
fn parse_colon_then_entries_is_refused() {
    let err = parse("[:, a: 1, b: 2]").unwrap_err();
    assert!(err.message.contains("[\"a\": 1, \"b\": 2]"), "{err}");
}

/// A tag names a variant, so it keys nothing: not a record, not a map,
/// not a pattern.
#[test]
fn parse_tag_key_errors_everywhere() {
    for src in [
        "[`dev: 8080]",
        "[\"x\": 1, `dev: 8080]",
        "[$k: 1, `dev: 2]",
        "let [`dev: p] = $x",
    ] {
        let err = parse(src).unwrap_err();
        assert!(
            err.message.contains("names a variant"),
            "{src}: {}",
            err.message
        );
    }
}

#[test]
fn parse_command_substitution() {
    let ast = unwrap_stmts(parse("let name = !{hostname}").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::Name("name".into())),
            value: Spanned::synthetic_boxed(Ast::Force(Spanned::synthetic_boxed(Ast::Block(
                body(vec![plain("hostname")])
            )),)),
        }]
    );
}

fn binary(l: Ast, op: BinaryOp, r: Ast) -> Ast {
    Ast::Binary(Spanned::synthetic_boxed(l), op, Spanned::synthetic_boxed(r))
}

#[test]
fn parse_arithmetic() {
    let sum = vec![binary(
        plain("2"),
        BinaryOp::Arith(ArithOp::Add),
        plain("3"),
    )];
    assert_eq!(unwrap_stmts(parse("$[2 + 3]").unwrap()), sum);
    assert_eq!(unwrap_stmts(parse("$[2+3]").unwrap()), sum);
}

#[test]
fn parse_arithmetic_precedence() {
    let ast = unwrap_stmts(parse("$[2 + 3 * 4]").unwrap());
    assert_eq!(
        ast,
        vec![binary(
            plain("2"),
            BinaryOp::Arith(ArithOp::Add),
            binary(plain("3"), BinaryOp::Arith(ArithOp::Mul), plain("4")),
        )]
    );
}

/// `$[…]` leaves no node of its own: a lone operand is that operand.
#[test]
fn expression_block_of_one_operand_is_the_operand() {
    let ast = unwrap_stmts(parse("$[1.5]").unwrap());
    assert_eq!(ast, vec![plain("1.5")]);
}

/// `not $x == 0` is `(not $x) == 0`, never `not ($x == 0)`.
#[test]
fn not_binds_tighter_than_binary_op() {
    let ast = unwrap_stmts(parse("$[not $x == 0]").unwrap());
    assert_eq!(
        ast,
        vec![binary(
            Ast::Not(Spanned::synthetic_boxed(Ast::Variable("x".into()))),
            BinaryOp::Eq(EqOp::Eq),
            plain("0"),
        )]
    );
}

/// `"!$d"` is the same force as `!$d`: a splice parses as the atom its
/// tokens spell outside the string, so no `!{$d}` thunk wraps the value.
#[test]
fn interpolated_force_of_a_variable_is_a_bare_force() {
    let ast = unwrap_stmts(parse("echo \"<!$d>\"").unwrap());
    assert_eq!(
        ast,
        vec![app(
            bare_head("echo"),
            vec![Ast::Interpolation(vec![
                sp(Ast::Literal("<".into())),
                sp(Ast::Force(Spanned::synthetic_boxed(Ast::Variable(
                    "d".into()
                )))),
                sp(Ast::Literal(">".into())),
            ])],
        )]
    );
}

/// `!` reaches over a dereference's keys but not over a block's: the
/// prelude's `!$p[tail]` forces the field, while `!{cmd}[k]` indexes the
/// forced result, inside a string or out.
#[test]
fn force_reaches_over_a_dereference_but_not_a_block() {
    let field = Ast::Force(Spanned::synthetic_boxed(Ast::Index {
        target: Spanned::synthetic_boxed(Ast::Variable("p".into())),
        keys: vec![sp(plain("tail"))],
    }));
    let result = Ast::Index {
        target: Spanned::synthetic_boxed(Ast::Force(Spanned::synthetic_boxed(Ast::Block(body(
            vec![plain("cmd")],
        ))))),
        keys: vec![sp(plain("k"))],
    };
    assert_eq!(
        unwrap_stmts(parse("!$p[tail]").unwrap()),
        vec![field.clone()]
    );
    assert_eq!(
        unwrap_stmts(parse("!{cmd}[k]").unwrap()),
        vec![result.clone()]
    );
    assert_eq!(
        unwrap_stmts(parse("\"!$p[tail]!{cmd}[k]\"").unwrap()),
        vec![Ast::Interpolation(vec![sp(field), sp(result)])]
    );
}

/// `$(name)` marks the end of a name and takes no `[key]`, inside a
/// string or out; every other splice is indexed by the keys after it.
#[test]
fn a_delimited_name_takes_no_postfix_keys() {
    for src in [
        "\"$h[file]\"",
        "\"!$h[file]\"",
        "\"!{h}[file]\"",
        "\"$[h][file]\"",
    ] {
        let ast = unwrap_stmts(parse(src).unwrap());
        let Ast::Interpolation(parts) = &ast[0] else {
            panic!("{src:?}: expected an interpolation, got {ast:?}");
        };
        assert_eq!(parts.len(), 1, "{src:?}: expected one part, got {parts:?}");
        // `!$h` reaches over its keys; `!{h}` is forced, then indexed.
        let (indexed, forced_outside) = match &parts[0].item {
            Ast::Force(inner) => (&*inner.item, true),
            other => (other, false),
        };
        let Ast::Index { target, keys } = indexed else {
            panic!("{src:?}: expected an index, got {:?}", parts[0].item);
        };
        assert_eq!(keys.len(), 1, "{src:?}");
        assert_eq!(forced_outside, src.starts_with("\"!$"), "{src:?}");
        assert_eq!(
            matches!(*target.item, Ast::Force(_)),
            src.starts_with("\"!{"),
            "{src:?}"
        );
    }

    let ast = unwrap_stmts(parse("\"$(h)[file]\"").unwrap());
    let Ast::Interpolation(parts) = &ast[0] else {
        panic!("expected an interpolation, got {ast:?}");
    };
    assert_eq!(parts[1].item, Ast::Literal("[file]".into()));

    for src in ["echo $(h)[file]", "echo !$(h)[file]"] {
        let err = parse(src).unwrap_err();
        assert!(
            matches!(err.kind, ParseErrorKind::Touching { .. }),
            "{src:?} must be refused as touching, got: {}",
            err.message
        );
    }
    for src in ["echo $h[file]", "echo !{h}[file]"] {
        let ast = unwrap_stmts(parse(src).unwrap());
        assert!(
            matches!(&ast[0], Ast::Call { args, .. } if matches!(args[0].item, Ast::Index { .. })),
            "{src:?}: expected an index, got {ast:?}"
        );
    }
}

#[test]
fn parse_index() {
    let ast = unwrap_stmts(parse("echo $items[0]").unwrap());
    assert_eq!(
        ast,
        vec![app(
            bare_head("echo"),
            vec![Ast::Index {
                target: Spanned::synthetic_boxed(Ast::Variable("items".into())),
                keys: vec![Spanned::synthetic(plain("0"))],
            }],
        )]
    );
}

#[test]
fn parse_postfix_index_on_list_literal() {
    let ast = unwrap_stmts(parse("return ['a'][0]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::Index {
            target: Spanned::synthetic_boxed(Ast::List(vec![ListElem::Single(sp(Ast::Literal(
                "a".into()
            ))),])),
            keys: vec![Spanned::synthetic(plain("0"))],
        },)))]
    );
}

#[test]
fn parse_interpolation() {
    let ast = unwrap_stmts(parse("echo \"hello $name\"").unwrap());
    assert_eq!(
        ast,
        vec![app(
            bare_head("echo"),
            vec![Ast::Interpolation(vec![
                sp(Ast::Literal("hello ".into())),
                sp(Ast::Variable("name".into())),
            ])],
        )]
    );
}

#[test]
fn parse_destructuring() {
    let ast = unwrap_stmts(parse("let [first, second] = [a, b]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::List {
                elems: vec![
                    Pattern::Name("first".into()),
                    Pattern::Name("second".into()),
                ],
                rest: None,
            }),
            value: Spanned::synthetic_boxed(Ast::List(vec![
                ListElem::Single(sp(plain("a"))),
                ListElem::Single(sp(plain("b"))),
            ])),
        }]
    );
}

#[test]
fn parse_rest_pattern() {
    let ast = unwrap_stmts(parse("let [head, ...rest] = $list").unwrap());
    match &ast[0] {
        Ast::Let { pattern, .. } => {
            assert_eq!(
                pattern.item,
                Pattern::List {
                    elems: vec![Pattern::Name("head".into())],
                    rest: Some("rest".into()),
                }
            );
        }
        _ => panic!("expected binding"),
    }
}

#[test]
fn rest_pattern_name_rejects_reserved_keyword() {
    let err = parse("let [...try] = $xs").unwrap_err();
    assert!(
        err.message.contains("reserved keyword"),
        "rest-pattern name should enforce the reserved-name guard: {err:?}"
    );
}

#[test]
fn pattern_rejects_duplicate_names() {
    for src in [
        "let [x, x] = [1, 2]",
        "let [x, [_, x]] = [1, [2, 3]]",
        "let [x, ...x] = [1, 2]",
        "let [a: x, b: x] = $m",
        "echo { |[x, x]| $x }",
    ] {
        let err = parse(src).expect_err("duplicate binding must not parse");
        assert!(
            err.message.contains("binds `x` more than once"),
            "{src:?}: {err:?}"
        );
    }
}

#[test]
fn curried_params_may_shadow() {
    assert!(parse("echo { |x x| $x }").is_ok());
}

#[test]
fn parse_wildcard_pattern() {
    let ast = unwrap_stmts(parse("let _ = hello").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::Wildcard),
            value: Spanned::synthetic_boxed(plain("hello")),
        }]
    );
}

#[test]
fn parse_wildcard_in_destructuring() {
    let ast = unwrap_stmts(parse("let [_, x] = [a, b]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::List {
                elems: vec![Pattern::Wildcard, Pattern::Name("x".into())],
                rest: None,
            }),
            value: Spanned::synthetic_boxed(Ast::List(vec![
                ListElem::Single(sp(plain("a"))),
                ListElem::Single(sp(plain("b"))),
            ])),
        }]
    );
}

#[test]
fn parse_command_with_lambda_arg() {
    let ast = unwrap_stmts(parse("for $items { |x| echo $x }").unwrap());
    match &ast[0] {
        Ast::Call { head, args, .. } => {
            assert_eq!(head, &bare_head("for"));
            assert_eq!(args.len(), 2); // $items and the lambda
            assert!(matches!(args[0].item, Ast::Variable(_)));
            assert!(matches!(args[1].item, Ast::Lambda { .. }));
        }
        _ => panic!("expected command"),
    }
}

#[test]
fn parse_spread_in_list() {
    let ast = unwrap_stmts(parse("return [...$a, b]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::List(
            vec![
                ListElem::Spread(sp(Ast::Variable("a".into()))),
                ListElem::Single(sp(plain("b"))),
            ]
        ),)))]
    );
}

#[test]
fn parse_empty_map() {
    let ast = unwrap_stmts(parse("[:]").unwrap());
    assert_eq!(ast, vec![Ast::Map(vec![])]);
}

#[test]
fn parse_empty_list() {
    let ast = unwrap_stmts(parse("[]").unwrap());
    assert_eq!(ast, vec![Ast::List(vec![])]);
}

#[test]
fn parse_quoted_key_with_spread() {
    let ast = unwrap_stmts(parse("[\"k\": 'v', ...$d]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Map(vec![
            MapEntry::Entry {
                key: sp(Ast::Literal("k".into())),
                value: sp(Ast::Literal("v".into())),
            },
            MapEntry::Spread(sp(Ast::Variable("d".into()))),
        ])]
    );
}

/// An interpolating key is computed, exactly as `$k` is.
#[test]
fn parse_interpolated_key_is_computed() {
    let ast = unwrap_stmts(parse("[\"$a-x\": 1]").unwrap());
    let [Ast::Map(entries)] = &ast[..] else {
        panic!("{ast:?}")
    };
    let [MapEntry::Entry { key, .. }] = &entries[..] else {
        panic!("{entries:?}")
    };
    assert!(matches!(key.item, Ast::Interpolation(_)), "{key:?}");
}

#[test]
fn parse_keyed_map_bare_element_errors() {
    assert!(parse("[\"k\": 1, 5]").is_err());
}

#[test]
fn parse_leading_spread_disambiguates_to_record() {
    // The `key: val` pair sits past the spread, where the lookahead has to
    // reach to call this a record.
    let ast = unwrap_stmts(parse("[...$d, k: 'v']").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Record(vec![
            RecordEntry::Spread(sp(Ast::Variable("d".into()))),
            RecordEntry::Field {
                key: Spanned::synthetic("k".into()),
                value: sp(Ast::Literal("v".into())),
            },
        ])]
    );
}

#[test]
fn a_records_spread_comes_first() {
    let err = parse("[k: 'v', ...$d]").unwrap_err();
    assert!(
        err.message.contains("a record's spread comes first"),
        "got: {}",
        err.message
    );
}

#[test]
fn a_second_spread_in_a_record_is_refused_whatever_the_order() {
    for src in ["[...$a, ...$b, k: 1]", "[k: 1, ...$a, ...$b]"] {
        let err = parse(src).unwrap_err();
        assert!(
            err.message.contains("one other record, not two"),
            "{src}: {}",
            err.message
        );
    }
}

/// The inner `]` of the spread operand must not be read as the outer
/// collection's close, or `[...[a: 1], b: 2]` would parse as a list.
#[test]
fn parse_leading_spread_of_nested_collection_disambiguates_to_record() {
    let ast = unwrap_stmts(parse("[...[a: 1], b: 2]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Record(vec![
            RecordEntry::Spread(sp(Ast::Record(vec![RecordEntry::Field {
                key: Spanned::synthetic("a".into()),
                value: sp(plain("1")),
            }]))),
            RecordEntry::Field {
                key: Spanned::synthetic("b".into()),
                value: sp(plain("2")),
            },
        ])]
    );
}

/// One computed key makes the whole literal a map, static keys and all.
#[test]
fn parse_computed_key_disambiguates_to_map() {
    let ast = unwrap_stmts(parse("[a: 1, $k: 2]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Map(vec![
            MapEntry::Entry {
                key: sp(Ast::Literal("a".into())),
                value: sp(plain("1")),
            },
            MapEntry::Entry {
                key: sp(Ast::Variable("k".into())),
                value: sp(plain("2")),
            },
        ])]
    );
}

/// Unterminated, the same lookahead must error rather than hang.
#[test]
fn parse_unterminated_leading_spread_errors_without_hang() {
    assert!(parse("[...[a: 1").is_err());
}

#[test]
fn parse_leading_spread_disambiguates_to_list() {
    // Past the spread sits a bare element, so this one is a list.
    let ast = unwrap_stmts(parse("[...$xs, a]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::List(vec![
            ListElem::Spread(sp(Ast::Variable("xs".into()))),
            ListElem::Single(sp(plain("a"))),
        ])]
    );
}

#[test]
fn parse_record_with_blocks() {
    // Standalone: a multiline record is a value, not a command.
    let src1 = "[\n    quit: { echo q },\n    help: { echo h },\n]";
    let ast1 = unwrap_stmts(parse(src1).unwrap());
    assert_eq!(ast1.len(), 1);
    assert!(matches!(&ast1[0], Ast::Record(_)));

    // And the same record in argument position.
    let src = "dispatch $action [\n    quit: { echo quitting },\n    help: { echo help },\n    _: { echo unknown },\n]";
    let ast = unwrap_stmts(parse(src).unwrap());
    match &ast[0] {
        Ast::Call { head, args, .. } => {
            assert_eq!(head, &bare_head("dispatch"));
            assert_eq!(args.len(), 2);
            assert!(matches!(&args[1].item, Ast::Record(_)));
        }
        _ => panic!("expected command, got {:?}", ast[0]),
    }
}

#[test]
fn parse_newline_separates_statements_in_block_inside_record() {
    let src = "return [prompt: { let x = hi\nreturn \"$x> \" }]";
    let ast = unwrap_stmts(parse(src).unwrap());
    match &ast[0] {
        Ast::Return(Some(val)) => match val.item.as_ref() {
            Ast::Record(entries) => {
                assert_eq!(entries.len(), 1);
                let RecordEntry::Field { value, .. } = &entries[0] else {
                    panic!("expected record field");
                };
                let Ast::Block(stmts) = &value.item else {
                    panic!("expected block in prompt entry");
                };
                assert!(matches!(stmts[0].item, Ast::Let { .. }));
                assert!(matches!(stmts[1].item, Ast::Return(Some(_))));
            }
            _ => panic!("expected record"),
        },
        _ => panic!("expected return record"),
    }
}

#[test]
fn parse_if_else_blocks_across_newline_with_explicit_else() {
    let src = "return [aliases: [ls: { |args| if $is-mac { echo a }\nelse { echo b } }]]";
    let ast = unwrap_stmts(parse(src).unwrap());
    let Ast::Return(Some(val)) = &ast[0] else {
        panic!("expected return");
    };
    let Ast::Record(entries) = val.item.as_ref() else {
        panic!("expected record");
    };
    let RecordEntry::Field {
        value: aliases_val, ..
    } = &entries[0]
    else {
        panic!("expected aliases entry");
    };
    let Ast::Record(alias_entries) = &aliases_val.item else {
        panic!("expected aliases record");
    };
    let RecordEntry::Field { value: ls_val, .. } = &alias_entries[0] else {
        panic!("expected ls entry");
    };
    let Ast::Lambda { body, .. } = &ls_val.item else {
        panic!("expected lambda");
    };
    assert_eq!(body.len(), 1);
    let Ast::If { branches, else_ } = &body[0].item else {
        panic!("expected Ast::If, got {:?}", body[0]);
    };
    assert_eq!(branches.len(), 1);
    assert!(matches!(branches[0].cond.item.as_ref(), Ast::Variable(s) if s == "is-mac"));
    assert!(matches!(branches[0].body.item.as_ref(), Ast::Block(_)));
    assert!(matches!(
        else_.as_ref().map(|b| b.item.as_ref()),
        Some(Ast::Block(_))
    ));
}

#[test]
fn parse_if_rejects_trailing_same_line_argument() {
    let err = parse("if true { echo yes } echo unexpected").unwrap_err();
    assert!(
        err.message.contains("unexpected") && err.message.contains("`if`"),
        "expected an unexpected-argument error naming `if`, got: {}",
        err.message
    );
}

#[test]
fn parse_if_still_allows_a_newline_separated_next_statement() {
    let ast = unwrap_stmts(parse("if true { echo yes }\necho after").unwrap());
    assert_eq!(ast.len(), 2);
    assert!(matches!(ast[0], Ast::If { .. }));
    assert_eq!(ast[1], app(bare_head("echo"), vec![plain("after")]));
}

#[test]
fn parse_if_still_allows_a_semicolon_separated_next_statement() {
    let ast = unwrap_stmts(parse("if true { echo yes }; echo after").unwrap());
    assert_eq!(ast.len(), 2);
    assert!(matches!(ast[0], Ast::If { .. }));
    assert_eq!(ast[1], app(bare_head("echo"), vec![plain("after")]));
}

/// `` case $r [`ok: { |v| echo $v }] `` — the one-armed fixture the
/// statement-boundary tests below share.
const ONE_ARMED_CASE: &str = "case $r [`ok: { |v| echo $v }]";

fn one_armed_case() -> Ast {
    Ast::Case {
        scrutinee: Spanned::synthetic_boxed(Ast::Variable("r".into())),
        arms: vec![CaseArm {
            tag: Spanned::synthetic("ok".into()),
            body: Spanned::synthetic_boxed(arm_lambda(
                Pattern::Name("v".into()),
                vec![app(bare_head("echo"), vec![Ast::Variable("v".into())])],
            )),
        }],
    }
}

/// An arm's own `{ |p| … }` is an ordinary lambda in the tree.
fn arm_lambda(param: Pattern, stmts: Vec<Ast>) -> Ast {
    Ast::Lambda {
        param: Spanned::synthetic(param),
        body: body(stmts),
    }
}

#[test]
fn parse_case_reads_a_tag_and_a_binder_per_arm() {
    let ast = unwrap_stmts(parse("case $r [`ok: { |v| echo $v }, `err: { |_| echo no }]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Case {
            scrutinee: Spanned::synthetic_boxed(Ast::Variable("r".into())),
            arms: vec![
                CaseArm {
                    tag: Spanned::synthetic("ok".into()),
                    body: Spanned::synthetic_boxed(arm_lambda(
                        Pattern::Name("v".into()),
                        vec![app(bare_head("echo"), vec![Ast::Variable("v".into())])],
                    )),
                },
                CaseArm {
                    tag: Spanned::synthetic("err".into()),
                    body: Spanned::synthetic_boxed(arm_lambda(
                        Pattern::Wildcard,
                        vec![app(bare_head("echo"), vec![plain("no")])],
                    )),
                },
            ],
        }]
    );
}

/// A destructuring binder is a pattern like any other, so a payload
/// record can be taken apart in the arm that receives it.
#[test]
fn parse_case_arm_binder_may_destructure() {
    let ast = unwrap_stmts(parse("case $s [`more: { |[head: h]| echo $h }]").unwrap());
    let Ast::Case { arms, .. } = &ast[0] else {
        panic!("expected a case");
    };
    let Ast::Lambda { param, .. } = arms[0].body.item.as_ref() else {
        panic!("expected an inline arm");
    };
    assert_eq!(
        param.item,
        Pattern::Map(vec![MapPatternEntry {
            key: "head".into(),
            pattern: Pattern::Name("h".into()),
        }])
    );
}

/// The set of alternatives is syntax; an arm's *body* is a computation
/// however it is spelled, so a function named elsewhere is an arm too.
#[test]
fn parse_case_arm_body_may_be_any_atom() {
    let ast = unwrap_stmts(parse("case $r [`ok: $handler, `err: !{ recover }]").unwrap());
    let Ast::Case { arms, .. } = &ast[0] else {
        panic!("expected a case");
    };
    assert_eq!(arms[0].body.item.as_ref(), &Ast::Variable("handler".into()));
    assert!(matches!(arms[1].body.item.as_ref(), Ast::Force(_)));
}

#[test]
fn parse_case_rejects_a_computed_table() {
    let err = parse("case $r $handlers").unwrap_err();
    assert!(
        err.message
            .contains("`case` wants its arms here, one per tag"),
        "expected the arms-are-syntax error, got: {}",
        err.message
    );
}

#[test]
fn parse_case_rejects_a_spread_arm() {
    let err = parse("case $r [`ok: { |v| echo $v }, ...$rest]").unwrap_err();
    assert!(
        err.message.contains("`...` spread has no meaning here"),
        "expected the spread-arm error, got: {}",
        err.message
    );
}

#[test]
fn parse_case_rejects_a_repeated_tag() {
    let err = parse("case $r [`ok: { |v| echo $v }, `ok: { |v| echo again }]").unwrap_err();
    assert!(
        err.message.contains("already has a `ok arm"),
        "expected the duplicate-arm error, got: {}",
        err.message
    );
}

#[test]
fn parse_case_rejects_an_unbound_arm_body() {
    let err = parse("case $r [`ok: { echo hi }]").unwrap_err();
    assert!(
        err.message.contains("must bind its payload"),
        "expected the missing-binder error, got: {}",
        err.message
    );
}

#[test]
fn parse_case_rejects_a_two_parameter_arm() {
    let err = parse("case $r [`ok: { |a b| echo hi }]").unwrap_err();
    assert!(
        err.message.contains("binds exactly one payload"),
        "expected the arity error, got: {}",
        err.message
    );
}

#[test]
fn parse_case_rejects_no_arms() {
    let err = parse("case $r []").unwrap_err();
    assert!(
        err.message.contains("at least one arm"),
        "expected the empty-case error, got: {}",
        err.message
    );
}

#[test]
fn parse_case_rejects_trailing_same_line_argument() {
    let err = parse(&format!("{ONE_ARMED_CASE} echo unexpected")).unwrap_err();
    assert!(
        err.message.contains("unexpected") && err.message.contains("`case`"),
        "expected an unexpected-argument error naming `case`, got: {}",
        err.message
    );
}

#[test]
fn parse_case_still_allows_a_newline_separated_next_statement() {
    let ast = unwrap_stmts(parse(&format!("{ONE_ARMED_CASE}\necho after")).unwrap());
    assert_eq!(
        ast,
        vec![
            one_armed_case(),
            app(bare_head("echo"), vec![plain("after")]),
        ]
    );
}

#[test]
fn parse_case_still_allows_a_semicolon_separated_next_statement() {
    let ast = unwrap_stmts(parse(&format!("{ONE_ARMED_CASE}; echo after")).unwrap());
    assert_eq!(
        ast,
        vec![
            one_armed_case(),
            app(bare_head("echo"), vec![plain("after")]),
        ]
    );
}

#[test]
fn parse_case_still_allowed_as_a_pipeline_stage() {
    let ast = unwrap_stmts(parse(&format!("{ONE_ARMED_CASE} | cat")).unwrap());
    assert_eq!(
        ast,
        vec![Ast::Pipeline(body(vec![one_armed_case(), plain("cat")]))]
    );
}

#[test]
fn parse_redirect() {
    let ast = sole_stmt("echo hello > out.txt");
    let (Ast::Call { args, .. }, redirects) = unwrap_redirected(&ast) else {
        panic!("expected a redirected command, got {ast:?}");
    };
    assert_eq!(args.len(), 1);
    assert!(redirects.stdout.is_some());
}

#[test]
fn parse_herestring_redirect() {
    assert_eq!(
        redirects_of("cat << #'body'#").stdin,
        Some(StdinSource::Here(Ast::Literal("body".into())))
    );
}

/// A here-string payload takes any value form a redirect operand does.
#[test]
fn parse_herestring_variable_payload() {
    assert_eq!(
        redirects_of("cat << $body").stdin,
        Some(StdinSource::Here(Ast::Variable("body".into())))
    );
}

/// The bash-heredoc reflex earns an error naming the raw-string form, not
/// a silent feed of the literal word `EOF`.
#[test]
fn herestring_bare_word_is_rejected() {
    for src in ["cat <<EOF", "cat << EOF"] {
        let err = parse(src).expect_err("bare word after `<<` must not parse");
        assert!(
            err.message.contains("ral has no heredocs") && err.message.contains("#' ... '#"),
            "for {src:?} got: {}",
            err.message
        );
    }
}

/// A path after `<<` gets the `< path` correction instead.
#[test]
fn herestring_path_word_is_rejected() {
    let err = parse("cat << ./body.txt").expect_err("path after `<<` must not parse");
    assert!(err.message.contains("use `< path`"), "got: {}", err.message);
}

/// The fd prefix is spelling: it picks a stream and is gone, and an
/// identity dup picks none.
#[test]
fn fd_prefixes_name_streams() {
    let redirects = redirects_of("cmd 1>&1 2>&2 2>&1 > o");
    assert!(matches!(redirects.stdout, Some((WriteMode::Write, _))));
    assert!(matches!(redirects.stderr, Some(StderrTarget::Stdout)));
    assert!(redirects.stdin.is_none());
}

/// `2>` streams, so the AST states that stderr is never atomic.
#[test]
fn stderr_write_is_a_stream() {
    assert!(matches!(
        redirects_of("cmd 2> e").stderr,
        Some(StderrTarget::File(WriteMode::Stream, _))
    ));
}

/// Every stream binds once; each clash says which, and the caret starts at
/// the second redirect's operator.
#[test]
fn a_second_binding_of_a_stream_is_refused() {
    let cases = [
        ("cmd < a << #'b'#", "standard input is fed twice"),
        ("cmd > a >> b", "standard output is redirected twice"),
        ("cmd 2> a 2> b", "standard error is redirected twice; which"),
        ("cmd 2> e 2>&1", "`2>&1` would override the `2>` before it"),
        (
            "cmd 2>&1 2> e",
            "`2>&1` already sends it with standard output",
        ),
        ("cmd 2>&1 2>&1", "`2>&1` is written twice"),
        (
            "try { a } { b } > a > b",
            "standard output is redirected twice",
        ),
    ];
    for (src, want) in cases {
        let err = parse(src).expect_err(src);
        assert!(err.message.contains(want), "{src:?} got: {}", err.message);
    }
}

#[test]
fn a_clash_span_covers_the_second_redirect() {
    let src = "cmd > a >> b";
    let span = parse(src).expect_err(src).span.expect("span");
    assert_eq!(&src[span.start as usize..span.end as usize], ">> b");
}

/// The same stream through `2>&1` and a file is one clash either way
/// round, and distinct streams never clash.
#[test]
fn distinct_streams_bind_in_any_order() {
    assert!(parse("cmd < i > o 2> e").is_ok());
    assert!(parse("cmd 2> e > o < i").is_ok());
    assert!(parse("cmd 2>&1 > o").is_ok());
}

/// `<<` always feeds stdin: fd 0 may be spelled out, another standard
/// stream errors here (an fd past 2 never leaves the lexer).
#[test]
fn herestring_fd_prefix() {
    assert!(matches!(
        redirects_of("cat 0<< #'x'#").stdin,
        Some(StdinSource::Here(_))
    ));
    let err = parse("cat 2<< #'x'#").expect_err("fd 2 herestring must not parse");
    assert!(
        err.message.contains("always feeds stdin"),
        "got: {}",
        err.message
    );
}

/// The one statement `src` parses to, spans and `Call` wrappers intact.
fn sole_stmt(src: &str) -> Ast {
    let mut stmts = parse(src).unwrap();
    assert_eq!(stmts.len(), 1, "{src}");
    stmts.remove(0).item
}

fn home_word() -> Ast {
    tilde_word(TildePath { suffix: None })
}

/// The value bound by the lone `let` in `src`, unstripped.
fn let_value(src: &str) -> Ast {
    match sole_stmt(src) {
        Ast::Let { value, .. } => *value.item,
        other => panic!("expected a let, got {other:?}"),
    }
}

#[test]
fn parse_tilde() {
    assert_eq!(sole_stmt("~"), home_word());
}

#[test]
fn parse_tilde_path_suffix() {
    let ast = unwrap_stmts(parse("~/foo/bar").unwrap());
    assert_eq!(
        ast,
        vec![tilde_word(TildePath {
            suffix: Some("/foo/bar".into()),
        })]
    );
}

#[test]
fn parse_tilde_before_a_name_is_a_plain_word() {
    let ast = unwrap_stmts(parse("echo ~bob").unwrap());
    assert_eq!(ast, vec![app(bare_head("echo"), vec![plain("~bob")])]);
}

#[test]
fn parse_let_of_lone_tilde_binds_the_tilde_word() {
    assert_eq!(let_value("let h = ~"), home_word());
}

/// `$[foo]` is the word `foo` as a value, never a command named `foo`.
#[test]
fn parse_expr_block_head_is_a_value() {
    assert_eq!(let_value("let x = $[foo]"), plain("foo"));
    assert_eq!(sole_stmt("$[~]"), home_word());
}

#[test]
fn parse_literal_head_with_args_is_a_value_head() {
    let ast = unwrap_stmts(parse("42 foo").unwrap());
    assert_eq!(ast, vec![app(value_head(plain("42")), vec![plain("foo")])]);
}

#[test]
fn parse_tilde_path_command_head_with_args() {
    let ast = unwrap_stmts(parse("~/bin/x a").unwrap());
    assert_eq!(
        ast,
        vec![app(
            Head::TildePath(TildePath {
                suffix: Some("/bin/x".into()),
            }),
            vec![plain("a")],
        )]
    );
}

#[test]
fn parse_tilde_path_command_head_without_args() {
    let ast = parse("~/.local/bin/claude").unwrap();
    match ast.as_slice() {
        [
            Stmt {
                item: Ast::Call { head, args, .. },
                ..
            },
        ] => {
            assert_eq!(args.as_slice(), []);
            assert_eq!(
                head,
                &Head::TildePath(TildePath {
                    suffix: Some("/.local/bin/claude".into()),
                })
            );
        }
        _ => panic!("expected zero-arg command app, got {ast:?}"),
    }
}

#[test]
fn parse_list_in_command_position_remains_value() {
    let ast = unwrap_stmts(parse("[1,2]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::List(vec![
            ListElem::Single(sp(plain("1"))),
            ListElem::Single(sp(plain("2"))),
        ])]
    );
}

#[test]
fn parse_external_name_rejected_for_path_head() {
    assert!(parse("^./script").is_err());
}

#[test]
fn parse_literal_path_head_without_args() {
    let ast = parse("./script").unwrap();
    match ast.as_slice() {
        [
            Stmt {
                item: Ast::Call { head, args, .. },
                ..
            },
        ] => {
            assert_eq!(args.as_slice(), []);
            assert_eq!(head, &path_head("./script"));
        }
        _ => panic!("expected zero-arg path app, got {ast:?}"),
    }
}

#[test]
fn parse_tilde_with_space_is_bare() {
    // The space cuts the tilde loose: `foo` is a second argument, not a
    // suffix on `~`.
    let ast = unwrap_stmts(parse("echo ~ foo").unwrap());
    assert_eq!(
        ast,
        vec![app(bare_head("echo"), vec![home_word(), plain("foo")],)]
    );
}

#[test]
fn parse_nested_blocks() {
    let ast = unwrap_stmts(parse("{ { echo inner } }").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Block(body(vec![Ast::Block(body(vec![app(
            bare_head("echo"),
            vec![plain("inner")]
        )]))]))]
    );
}

// ── Recursion-depth chokepoints ─────────────────────────────────────
//
// These cover the two sub-grammars that do not route through
// `parse_primary` and so guard themselves.  Each uses a depth well past
// the cap but far short of any real stack ceiling, so a lost guard shows
// up as a missing error rather than a crash.

#[test]
fn deeply_nested_pattern_hits_nesting_cap() {
    let n = 200;
    let src = format!("let {}a{} = x", "[".repeat(n), "]".repeat(n));
    let err = parse(&src).unwrap_err();
    assert!(
        err.message.contains("too deep"),
        "deep pattern nesting should hit the cap, got: {}",
        err.message
    );
}

#[test]
fn deeply_nested_unary_minus_hits_nesting_cap() {
    let src = format!("$[{}1]", "- ".repeat(200));
    let err = parse(&src).unwrap_err();
    assert!(
        err.message.contains("too deep"),
        "deep unary-minus nesting should hit the cap, got: {}",
        err.message
    );
}

#[test]
fn deeply_nested_not_hits_nesting_cap() {
    let src = format!("$[{}$x]", "not ".repeat(200));
    let err = parse(&src).unwrap_err();
    assert!(
        err.message.contains("too deep"),
        "deep `not` nesting should hit the cap, got: {}",
        err.message
    );
}

#[test]
fn parse_force_stmt_still_allowed() {
    let ast = unwrap_stmts(parse("!{echo hello}").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Force(Spanned::synthetic_boxed(Ast::Block(body(
            vec![app(bare_head("echo"), vec![plain("hello")])]
        ),)))]
    );
}

#[test]
fn bare_bang_is_not_a_literal_word() {
    assert!(parse("echo !").is_err());
}

#[test]
fn let_rhs_on_next_line() {
    let ast = unwrap_stmts(parse("let x =\necho hi").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::Name("x".into())),
            value: Spanned::synthetic_boxed(app(bare_head("echo"), vec![plain("hi")],)),
        }]
    );
}

#[test]
fn let_rhs_on_next_line_multiple_newlines() {
    let ast = unwrap_stmts(parse("let x =\n\necho hi").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::Name("x".into())),
            value: Spanned::synthetic_boxed(app(bare_head("echo"), vec![plain("hi")],)),
        }]
    );
}

#[test]
fn let_destructure_rhs_on_next_line() {
    let ast = unwrap_stmts(parse("let [a, b] =\n[1, 2]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::List {
                elems: vec![Pattern::Name("a".into()), Pattern::Name("b".into())],
                rest: None,
            }),
            value: Spanned::synthetic_boxed(Ast::List(vec![
                ListElem::Single(sp(plain("1"))),
                ListElem::Single(sp(plain("2"))),
            ])),
        }]
    );
}

#[test]
fn let_rhs_chain_continues_before_question() {
    let ast = unwrap_stmts(parse("let x = echo hi\n? echo bye").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Let {
            pattern: Spanned::synthetic(Pattern::Name("x".into())),
            value: Spanned::synthetic_boxed(Ast::Chain(vec![
                sp(app(bare_head("echo"), vec![plain("hi")])),
                sp(app(bare_head("echo"), vec![plain("bye")])),
            ])),
        }]
    );
}

#[test]
fn pipeline_continuation_after_pipe() {
    let ast = unwrap_stmts(parse("echo hello |\nupper").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Pipeline(body(vec![
            app(bare_head("echo"), vec![plain("hello")]),
            plain("upper"),
        ]))]
    );
}

#[test]
fn pipeline_continuation_before_pipe() {
    let ast = unwrap_stmts(parse("echo hello\n| upper").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Pipeline(body(vec![
            app(bare_head("echo"), vec![plain("hello")]),
            plain("upper"),
        ]))]
    );
}

#[test]
fn newline_terminates_command_args() {
    // Two statements, not one command with two arguments.
    let ast = unwrap_stmts(parse("echo hello\nworld").unwrap());
    assert_eq!(ast.len(), 2);
}

#[test]
fn caret_is_not_a_continuation_token() {
    assert!(!needs_continuation("^"));
}

#[test]
fn needs_continuation_on_unterminated_string() {
    assert!(needs_continuation("\"foo"));
}

#[test]
fn needs_continuation_on_unterminated_string_with_inner_force() {
    // Nested unclosed forms are still one verdict, not a competing pair.
    assert!(needs_continuation("\"foo !{cmd"));
}

#[test]
fn complete_program_does_not_need_continuation() {
    assert!(!needs_continuation("echo done"));
}

/// The lexer calls an unbalanced top-level `{` or `[` an unterminated
/// delimiter, which reaches the REPL as a request for another line.
#[test]
fn needs_continuation_on_unbalanced_open_delimiters() {
    assert!(needs_continuation("let f = {"));
    assert!(needs_continuation("return [a, b"));
    assert!(needs_continuation("if true {"));
}

#[test]
fn balanced_delimiters_do_not_need_continuation() {
    assert!(!needs_continuation("let f = { return 1 }"));
    assert!(!needs_continuation("return [a, b]"));
}

/// A comment running to end of input must not mask the open delimiter
/// before it.
#[test]
fn needs_continuation_on_open_delim_then_comment_to_eof() {
    assert!(needs_continuation("let f = {# comment"));
    assert!(needs_continuation("return [a, b # comment"));
    assert!(needs_continuation("{# comment"));
    assert!(needs_continuation("[# comment"));
}

#[test]
fn balanced_program_with_trailing_comment_does_not_need_continuation() {
    assert!(!needs_continuation("let f = { return 1 } # done"));
    assert!(!needs_continuation("echo done # done"));
}

#[test]
fn needs_continuation_on_let_awaiting_rhs() {
    assert!(needs_continuation("let x ="));
    assert!(needs_continuation("let [a, b] ="));
}

/// A trailing `=` outside a binder is a plain-word argument, and a line
/// that already parses must never ask for another.
#[test]
fn trailing_bare_equals_does_not_need_continuation() {
    assert!(parse("x =").is_ok());
    assert!(!needs_continuation("x ="));
    assert!(parse("echo a =").is_ok());
    assert!(!needs_continuation("echo a ="));
}

#[test]
fn needs_continuation_on_dangling_continuation_token() {
    assert!(needs_continuation("echo hi |"));
    assert!(needs_continuation("echo a ?"));
    assert!(needs_continuation("if"));
    assert!(needs_continuation("if true x\nelsif"));
    assert!(needs_continuation("if true x\nelse"));
    // Condition parsed, body demanded, input gone.
    assert!(needs_continuation("if $c"));
    assert!(needs_continuation("if true a\nelsif $c"));
}

#[test]
fn if_same_line_bare_block_is_error() {
    // A third block on the same line wants the `else` keyword.
    let err = parse("if $c { a } { b }").unwrap_err();
    assert!(
        err.message.contains("else"),
        "error should hint at `else`: {err:?}"
    );
}

#[test]
fn if_newline_block_is_valid() {
    // On the next line it is a statement of its own.
    assert!(parse("if $c { a }\n{ b }").is_ok());
}

#[test]
fn if_with_else_keyword_is_valid() {
    assert!(parse("if $c { a } else { b }").is_ok());
}

/// `else` and `elsif` continue an `if`; as a stage head they have nothing
/// to continue.
#[test]
fn else_and_elsif_are_refused_as_stage_heads() {
    for (src, word) in [
        ("else { b }", "else"),
        ("elsif $c { b }", "elsif"),
        ("echo hi\nelse { b }", "else"),
        ("echo hi | elsif $c { b }", "elsif"),
    ] {
        let err = parse(src).unwrap_err();
        assert!(
            err.message
                .contains(&format!("`{word}` continues an `if` on the line above")),
            "{src:?}: {}",
            err.message
        );
    }
}

// ── Control operators (try / guard / within / grant / audit) ────────

fn unwrap_single_scope(ast: Vec<Stmt>) -> ScopeAst {
    let stripped: Vec<_> = ast.into_iter().map(|s| s.item).collect();
    match stripped.as_slice() {
        [Ast::Scope(op)] => op.clone(),
        _ => panic!("expected a single Ast::Scope, got {stripped:?}"),
    }
}

/// The scope and trailing redirects of the one statement `src` parses to.
fn redirected_scope(src: &str) -> (ScopeAst, Redirects<Ast>) {
    let ast = sole_stmt(src);
    let (Ast::Scope(op), redirects) = unwrap_redirected(&ast) else {
        panic!("expected a redirected scope, got {ast:?}");
    };
    (op.clone(), redirects.clone())
}

fn unwrap_single_exec(ast: Vec<Stmt>) -> (Head, Vec<Ast>) {
    let stripped: Vec<_> = ast.into_iter().map(|s| s.item).collect();
    match stripped.as_slice() {
        [Ast::Call { head, args }] => (head.clone(), args.iter().map(|s| s.item.clone()).collect()),
        _ => panic!("expected a single Ast::Call, got {stripped:?}"),
    }
}

#[test]
fn parse_try_two_blocks() {
    let op = unwrap_single_scope(parse("try { body } { handler }").unwrap());
    match op {
        ScopeAst::Try { body, handler } => {
            assert!(matches!(*body, Ast::Block(_)));
            assert!(matches!(*handler, Ast::Block(_)));
        }
        _ => panic!("expected ScopeAst::Try, got {op:?}"),
    }
}

#[test]
fn parse_try_body_then_lambda() {
    // The shape the prelude writes: a bound body, a lambda handler.
    let op = unwrap_single_scope(parse("try $body { |err| return () }").unwrap());
    match op {
        ScopeAst::Try { body, handler } => {
            assert!(matches!(*body, Ast::Variable(ref n) if n == "body"));
            assert!(matches!(*handler, Ast::Lambda { .. }));
        }
        _ => panic!("expected ScopeAst::Try, got {op:?}"),
    }
}

#[test]
fn parse_try_with_trailing_redirect() {
    let (op, redirects) = redirected_scope("try { body } { handler } > out");
    assert!(redirects.stdout.is_some());
    match op {
        ScopeAst::Try { body, handler } => {
            assert!(matches!(*body, Ast::Block(_)));
            assert!(matches!(*handler, Ast::Block(_)));
        }
        _ => panic!("expected ScopeAst::Try, got {op:?}"),
    }
}

#[test]
fn parse_try_with_two_trailing_redirects() {
    let (op, redirects) = redirected_scope("try { body } { handler } > out 2>&1");
    assert!(redirects.stdout.is_some());
    assert!(matches!(redirects.stderr, Some(StderrTarget::Stdout)));
    assert!(matches!(op, ScopeAst::Try { .. }));
}

#[test]
fn parse_if_case_and_return_take_trailing_redirects() {
    let ast = sole_stmt("if $c {a} else {b} > f");
    assert!(matches!(unwrap_redirected(&ast), (Ast::If { .. }, r) if r.stdout.is_some()));
    let ast = sole_stmt("case $x [`a: { |v| b }] 2> e");
    assert!(matches!(unwrap_redirected(&ast), (Ast::Case { .. }, r) if r.stderr.is_some()));
    let ast = sole_stmt("return 1 > f");
    assert!(matches!(unwrap_redirected(&ast), (Ast::Return(Some(_)), r) if r.stdout.is_some()));
}

#[test]
fn parse_value_head_with_redirect_keeps_its_stage() {
    let ast = sole_stmt("$f > out");
    assert!(matches!(unwrap_redirected(&ast), (Ast::Variable(_), r) if r.stdout.is_some()));
}

#[test]
fn parse_try_rejects_trailing_argument_after_redirect() {
    let err = parse("try { body } { handler } > out.txt oops").unwrap_err();
    assert!(
        err.message.contains("unexpected") && err.message.contains("`try`"),
        "expected an unexpected-argument error naming `try`, got: {}",
        err.message
    );
}

#[test]
fn parse_try_one_arg_is_error() {
    let err = parse("try { body }").unwrap_err();
    assert!(
        err.message.contains("try requires 2") && err.message.contains("got 1"),
        "unexpected message: {err}"
    );
}

#[test]
fn parse_try_zero_args_is_error() {
    let err = parse("try").unwrap_err();
    assert!(
        err.message.contains("try requires 2") && err.message.contains("got 0"),
        "unexpected message: {err}"
    );
}

#[test]
fn parse_try_three_args_is_error() {
    let err = parse("try { a } { b } { c }").unwrap_err();
    assert!(
        err.message.contains("try requires 2") && err.message.contains("got 3"),
        "unexpected message: {err}"
    );
}

#[test]
fn parse_guard_two_blocks() {
    let op = unwrap_single_scope(parse("guard { body } { cleanup }").unwrap());
    assert!(matches!(op, ScopeAst::Guard { .. }));
}

#[test]
fn parse_within_opts_and_body() {
    let op = unwrap_single_scope(parse("within [dir: '/tmp'] { body }").unwrap());
    match op {
        ScopeAst::Within { opts, body, .. } => {
            assert_eq!(opts.len(), 1);
            assert!(matches!(*body, Ast::Block(_)));
        }
        _ => panic!("expected ScopeAst::Within, got {op:?}"),
    }
}

/// The form's bracket is the form's own: `[]` there is the empty option
/// set rather than the empty list, which is what makes `grant [] { … }`
/// mean what it reads as.
#[test]
fn an_empty_option_bracket_is_no_options() {
    for src in ["within [] { body }", "grant [] { body }"] {
        let op = unwrap_single_scope(parse(src).unwrap());
        let opts = match op {
            ScopeAst::Within { opts, .. } | ScopeAst::Grant { caps: opts, .. } => opts,
            other => panic!("expected an option-taking form, got {other:?}"),
        };
        assert!(opts.is_empty(), "in {src}");
    }
}

/// `[:]` is a map, whose keys are data, so it names no options at all.
#[test]
fn a_map_bracket_is_not_an_option_list() {
    let err = parse("within [:] { body }").unwrap_err();
    assert!(
        err.message.contains("write `[]` for no options"),
        "unexpected message: {err}"
    );
}

/// An option is named, so a computed key cannot be one.
#[test]
fn a_computed_option_key_is_refused() {
    let err = parse("within [$k: 1] { body }").unwrap_err();
    assert!(
        err.message.contains("are named in writing"),
        "unexpected message: {err}"
    );
}

/// The option names are written in the form's bracket: a bundle bound
/// elsewhere and a spread into the bracket both hide them.
#[test]
fn a_bound_or_spread_option_bundle_is_refused() {
    for src in [
        "within $o { body }",
        "grant $o { body }",
        "within (mk) { body }",
        "within [dir: 'x', ...$rest] { body }",
        "grant [...$rest] { body }",
    ] {
        let err = parse(src).unwrap_err();
        assert!(
            err.message
                .contains("takes its options written in its own bracket"),
            "unexpected message for {src}: {err}"
        );
    }
}

/// Under a bracket that is no record literal, a repeated label has no
/// `DuplicateField` downstream to catch it.
#[test]
fn a_repeated_option_is_refused() {
    for src in [
        "within [dir: 'a', dir: 'b'] { body }",
        "grant [net: true, net: false] { body }",
    ] {
        let err = parse(src).unwrap_err();
        assert!(
            err.message.contains("is written twice in"),
            "unexpected message for {src}: {err}"
        );
    }
}

/// `handlers:` binds the names it lists in the body, so the list is
/// syntax: a table assembled elsewhere could never spell them.
#[test]
fn a_computed_handler_table_is_refused() {
    for src in [
        "within [handlers: $hs] { body }",
        "within [handlers: [\"foo\": { echo }]] { body }",
        "within [handlers: [...$hs]] { body }",
    ] {
        let err = parse(src).unwrap_err();
        assert!(
            err.message.contains("handlers:"),
            "unexpected message for {src}: {err}"
        );
    }
    // `[:]` spelled the empty set while the options were a map, and does
    // not simply stop parsing: it says where the empty set went.
    let err = parse("within [handlers: [:]] { body }").unwrap_err();
    assert!(
        err.message
            .contains("the empty handler set is `handlers: []`"),
        "unexpected message: {err}"
    );
}

/// The arms leave the options bracket: their labels are names, not data.
/// `[]` keeps the empty handler set.
#[test]
fn handler_arms_are_lifted_out_of_the_options() {
    let op = unwrap_single_scope(
        parse("within [dir: '/tmp', handlers: [deploy: { echo hi }]] { body }").unwrap(),
    );
    match op {
        ScopeAst::Within { opts, handlers, .. } => {
            assert_eq!(
                handlers.as_deref().map(<[HandlerArm]>::len),
                Some(1),
                "the one arm is lifted out"
            );
            assert_eq!(opts.len(), 1, "only `dir` is left among the options");
        }
        other => panic!("expected ScopeAst::Within, got {other:?}"),
    }
    let op = unwrap_single_scope(parse("within [handlers: []] { body }").unwrap());
    match op {
        ScopeAst::Within { handlers, .. } => assert_eq!(handlers, Some(Vec::new())),
        other => panic!("expected ScopeAst::Within, got {other:?}"),
    }
}

/// One name, one arm — as `case` refuses a repeated tag.
#[test]
fn a_repeated_handler_name_is_refused() {
    let err = parse("within [handlers: [foo: { echo a }, foo: { echo b }]] { body }").unwrap_err();
    assert!(
        err.message.contains("already has an arm"),
        "unexpected message: {err}"
    );
}

#[test]
fn parse_grant_caps_and_body() {
    let op = unwrap_single_scope(parse("grant [exec: [:]] { body }").unwrap());
    assert!(matches!(op, ScopeAst::Grant { .. }));
}

#[test]
fn parse_audit_one_block() {
    let op = unwrap_single_scope(parse("audit { body }").unwrap());
    assert!(matches!(op, ScopeAst::Audit { .. }));
}

#[test]
fn parse_audit_two_args_is_error() {
    let err = parse("audit a b").unwrap_err();
    assert!(
        err.message.contains("audit requires 1") && err.message.contains("got 2"),
        "msg: {err}"
    );
}

#[test]
fn parse_audit_zero_args_is_error() {
    let err = parse("audit").unwrap_err();
    assert!(err.message.contains("audit requires 1"), "msg: {err}");
}

#[test]
fn parse_audit_with_trailing_redirect() {
    let (op, redirects) = redirected_scope("audit { body } > out");
    assert!(matches!(op, ScopeAst::Audit { .. }));
    assert!(redirects.stdout.is_some());
}

// ── Reserved-name binding rejection ─────────────────────────────────

#[test]
fn parse_let_try_rejected() {
    let err = parse("let try = 1").unwrap_err();
    assert!(err.message.contains("'try'"), "msg: {err}");
}

#[test]
fn parse_let_within_rejected() {
    let err = parse("let within = 1").unwrap_err();
    assert!(err.message.contains("'within'"), "msg: {err}");
}

#[test]
fn parse_lambda_param_named_try_rejected() {
    assert!(parse("let f = { |try| 1 }").is_err());
}

// ── ^try keeps external-only semantics ──────────────────────────────

#[test]
fn parse_external_try_still_valid() {
    let (head, args) = unwrap_single_exec(parse("^try arg").unwrap());
    assert_eq!(head, external_head("try"));
    assert_eq!(args.len(), 1);
}

// ── Standalone redirect rejection ───────────────────────────────────

#[test]
fn parse_leading_redirect_after_newline_rejected() {
    let err = parse("echo hi\n> out").unwrap_err();
    assert!(
        err.message.contains("redirect must follow a command"),
        "msg: {err}"
    );
}

// ── Targeted diagnostics ────────────────────────────────────────────

/// Every program is refused, and its message contains what is paired with it.
fn refused_saying(cases: &[(&str, &str)]) {
    for (src, said) in cases {
        let err = parse(src).expect_err(src);
        assert!(
            err.message.contains(said),
            "{src:?} must say {said:?}, got: {}",
            err.message
        );
    }
}

/// A closer that closes nothing open is the lexer's definite error, so the
/// REPL does not wait for the line that cannot repair it.
#[test]
fn a_mismatched_closer_never_asks_for_more_input() {
    assert!(!needs_continuation("{ ]"));
    refused_saying(&[("{ ]", "mismatched `]`: the innermost open delimiter is `{`")]);
}

#[test]
fn a_function_header_opens_on_the_brace_line() {
    refused_saying(&[
        (
            "let f = {\n|x| echo $x }",
            "a function's parameters open on the same line as `{`: write `{ |x|`",
        ),
        ("{ # header\n|x| 1 }", "open on the same line as `{`"),
    ]);
}

#[test]
fn an_unclosed_parameter_list_names_the_closing_bar() {
    refused_saying(&[
        (
            "{ |x }",
            "expected `|` to close the parameter list: `{ |x| … }`",
        ),
        (
            "{ |x\n echo $x }",
            "expected `|` to close the parameter list",
        ),
        (
            "{ |x; echo $x }",
            "expected `|` to close the parameter list",
        ),
    ]);
}

#[test]
fn a_spread_with_nothing_after_it_says_what_it_spreads() {
    let said = "`...` spreads the value after it: write `...$xs`";
    refused_saying(&[
        ("echo ...", said),
        ("echo ...\necho a", said),
        ("echo ... | cat", said),
        ("echo ... ? a", said),
    ]);
}

#[test]
fn a_redirect_with_nothing_after_it_says_what_it_needs() {
    let write = "`>` needs a file to write to";
    let read = "`<` needs a file to read";
    let feed = "`<<` needs a string to feed";
    refused_saying(&[
        ("echo >", write),
        ("echo >>\n", write),
        ("echo 2>", write),
        ("echo > | cat", write),
        ("cat <", read),
        ("cat <\necho a", read),
        ("cat <<", feed),
        ("cat <<\necho a", feed),
    ]);
}

#[test]
fn an_index_without_a_key_gives_the_two_forms() {
    let said = "an index needs a key: `$x[name]` or `$x[0]`";
    refused_saying(&[
        ("echo $x[]", said),
        ("echo $x[a][]", said),
        ("echo \"$x[]\"", said),
    ]);
}

#[test]
fn an_empty_expression_block_gives_examples() {
    let said = "`$[…]` holds an expression: `$[1 + 2]`, `$[$n > 0]`";
    refused_saying(&[("$[]", said), ("echo $[ ]", said), ("echo \"$[]\"", said)]);
}

#[test]
fn an_operator_with_nothing_after_it_says_which() {
    refused_saying(&[
        ("$[1 +]", "`+` needs an operand on its right"),
        ("$[$a && ]", "`&&` needs an operand on its right"),
        ("$[$a == ]", "`==` needs an operand on its right"),
        ("$[$a <= ]", "`<=` needs an operand on its right"),
        ("$[-]", "`-` needs an operand"),
        ("$[1 + -]", "`-` needs an operand"),
        ("$[not]", "`not` needs an operand"),
    ]);
}

#[test]
fn a_stage_with_no_command_says_what_precedes_it() {
    let pipe = "a pipeline needs a command before `|`";
    let question = "`?` needs a command before it to fall back from";
    refused_saying(&[
        ("| cat", pipe),
        ("echo a ? | cat", pipe),
        ("? echo a", question),
        ("echo a | ? b", question),
        ("|| cat", "ral has no `||`"),
    ]);
}

#[test]
fn a_rest_pattern_comes_last() {
    let said = "`...rest` takes the remaining elements, so it comes last";
    refused_saying(&[
        ("let [...r, a] = $x", said),
        ("let [a, ...r, b] = $x", said),
        ("let [a, ...r b] = $x", said),
    ]);
    for src in ["let [a, ...r] = $x", "let [a, ...r,] = $x"] {
        assert!(parse(src).is_ok(), "{src:?} must parse");
    }
}

#[test]
fn a_return_before_a_statement_keyword_says_why() {
    refused_saying(&[
        (
            "return if $c { a } else { b }",
            "`return` takes one value, and `if` begins a statement: put the `return` \
             inside each branch, or write `return !{if …}`",
        ),
        (
            "return case $x [`a: { |v| 1 }]",
            "and `case` begins a statement",
        ),
        ("return let x = 1", "and `let` begins a statement"),
        ("return try { a } { b }", "or write `return !{try …}`"),
    ]);
}

#[test]
fn a_glued_equals_in_a_binding_says_to_space_it() {
    refused_saying(&[
        (
            "let x=5",
            "`x=5` is one word: `let` wants spaces around `=`, as in `let x = 5`",
        ),
        ("let x= 5", "`x=` is one word"),
        ("let x =5", "`=5` is one word"),
        ("let dir=/tmp/x", "`dir=/tmp/x` is one word"),
        ("let x foo", "expected '=' after the binding name"),
    ]);
}

#[test]
fn a_key_with_its_colon_glued_to_its_value_says_to_space_it() {
    refused_saying(&[
        (
            "echo [host:\"h\"]",
            "`host:` is one word; for the key `host`, put a space after the colon: \
             `host: value`",
        ),
        (
            "echo [a: 1, port:$p]",
            "`port:` is one word; for the key `port`",
        ),
        (
            "within [dir:$d] { a }",
            "`dir:` is one word; for the key `dir`",
        ),
    ]);
    assert!(parse("echo [http://x, y]").is_ok());
}

#[test]
fn an_option_bracket_ends_its_unit() {
    for src in [
        "within [dir: $d]{ a }",
        "within []{ a }",
        "grant [net: $n]{ a }",
    ] {
        let err = parse(src).expect_err(src);
        assert!(
            err.message.contains("words touch"),
            "{src:?}: {}",
            err.message
        );
    }
}

#[test]
fn else_and_elsif_end_their_unit() {
    for src in ["if $c {a} else{b}", "if $c {a} elsif$d {b}"] {
        let err = parse(src).expect_err(src);
        assert!(
            err.message.contains("words touch"),
            "{src:?}: {}",
            err.message
        );
    }
}

#[test]
fn a_forced_variable_touching_an_atom_is_reported_whole() {
    let err = parse("f !$x'a'").unwrap_err();
    let ParseErrorKind::Touching { first, .. } = err.kind else {
        panic!("expected a touching error, got {err:?}");
    };
    let src = "f !$x'a'";
    assert_eq!(&src[first.start as usize..first.end as usize], "!$x");
}
