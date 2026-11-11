use super::*;
use crate::ir::{CompareOp, EqOp, StderrTarget};
use crate::path::TildePath;
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
        head: Spanned::synthetic(head),
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
        Ast::Call { head, args } if args.is_empty() => match head.item {
            Head::Bare(s) => plain(&s),
            Head::Path(s) => Ast::Word(Word::Path(s)),
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
            keys: keys
                .into_iter()
                .map(|key| {
                    Spanned::synthetic(match key.item {
                        IndexKey::Data(ast) => IndexKey::Data(Box::new(strip_one(*ast))),
                        other => other,
                    })
                })
                .collect(),
        },
        Ast::Call { head, args } => {
            let plain_args: Vec<Ast> = args.into_iter().map(|sp| sp.item).collect();
            app(strip_head(head.item), strip_args(plain_args))
        }
        Ast::Redirected { stage, redirects } => match *stage.item {
            Ast::Call { head, args } => {
                let plain_args: Vec<Ast> = args.into_iter().map(|sp| sp.item).collect();
                app_redir(strip_head(head.item), strip_args(plain_args), redirects)
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
        ScopeAst::Guard {
            acquire,
            body,
            release,
        } => ScopeAst::Guard {
            acquire: s(acquire),
            body: s(body),
            release: s(release),
        },
        ScopeAst::Within { opts, body } => ScopeAst::Within {
            opts: strip_opts(opts),
            body: s(body),
        },
        ScopeAst::Grant { caps, body } => ScopeAst::Grant {
            caps: strip_opts(caps),
            body: s(body),
        },
        ScopeAst::Audit { body } => ScopeAst::Audit { body: s(body) },
        ScopeAst::Spawn { title, body } => ScopeAst::Spawn {
            title: title.map(|t| Box::new(Spanned::synthetic(strip_one(t.item)))),
            body: s(body),
        },
        ScopeAst::Gather { body } => ScopeAst::Gather { body: s(body) },
    }
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
            assert_eq!(&head.item, &external_head("git"));
        }
        _ => panic!("expected zero-arg external-name app, got {ast:?}"),
    }
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
        parse(src).expect_err("`&` must not parse");
    }
}

#[test]
fn parse_let_after_pipe_rejected() {
    parse("cmd | let x = y").unwrap_err();
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

    parse("return $[< 3]").unwrap_err();
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
    parse("echo a && echo b").unwrap_err();
    parse("echo a || echo b").unwrap_err();
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
        ("$[-5'a']", "-5", "'a'", "-5'a'"),
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
    for src in [
        "^ ls",
        "echo ... $x",
        "echo [... $x]",
        "let [a, ... r] = $x",
        "echo ! $x",
    ] {
        parse(src).unwrap_err();
    }
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
fn parse_quoted_key_is_a_label() {
    let ast = unwrap_stmts(parse("[\"a\": 1, 'b': true]").unwrap());
    assert_eq!(
        ast,
        vec![Ast::Record(vec![
            RecordEntry::Field {
                key: Spanned::synthetic("a".into()),
                value: sp(plain("1")),
            },
            RecordEntry::Field {
                key: Spanned::synthetic("b".into()),
                value: sp(plain("true")),
            },
        ])]
    );
}

#[test]
fn parse_colon_opens_a_map_on_written_keys() {
    let ast = unwrap_stmts(parse("[:, a: 1, b: 2]").unwrap());
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

/// A tag names a variant, so it keys nothing: not a record, not a map,
/// not a pattern.
#[test]
fn parse_tag_key_errors_everywhere() {
    for src in [
        "[`dev: 8080]",
        "[:, x: 1, `dev: 8080]",
        "[$k: 1, `dev: 2]",
        "let [`dev: p] = $x",
    ] {
        parse(src).unwrap_err();
    }
}

fn binary(l: Ast, op: BinaryOp, r: Ast) -> Ast {
    Ast::Binary(Spanned::synthetic_boxed(l), op, Spanned::synthetic_boxed(r))
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

/// Where a value is due inside `$[…]`, a `+` or `-` touching a numeral is
/// its sign, so the word is the one it is outside: `i64::MIN` included.
#[test]
fn a_sign_touching_a_numeral_is_its_sign() {
    let neg = |a: Ast| Ast::Negate(Spanned::synthetic_boxed(a));
    let tag = |payload: Ast| Ast::Tag {
        label: "a".into(),
        payload: Some(Spanned::synthetic_boxed(payload)),
    };
    for (src, want) in [
        ("$[-9223372036854775808]", plain("-9223372036854775808")),
        ("$[-9223372036854775809]", plain("-9223372036854775809")),
        ("$[+5]", plain("+5")),
        ("$[+1.5]", plain("+1.5")),
        ("$[-.5]", plain("-.5")),
        ("$[-1.5e+3]", plain("-1.5e+3")),
        ("$[(-1)]", plain("-1")),
        (
            "$[2*-3]",
            binary(plain("2"), BinaryOp::Arith(ArithOp::Mul), plain("-3")),
        ),
        ("$[--5]", neg(plain("-5"))),
        ("$[-+5]", neg(plain("+5"))),
        ("$[- 5]", neg(plain("5"))),
        ("$[`a -1]", tag(plain("-1"))),
    ] {
        assert_eq!(unwrap_stmts(parse(src).unwrap()), vec![want], "{src:?}");
    }
}

/// After a complete operand `-` is binary, however it is spaced, whatever
/// the operand.
#[test]
fn a_sign_after_a_value_is_binary() {
    let nullary = |l: &str| Ast::Tag {
        label: l.into(),
        payload: None,
    };
    for (src, left, right) in [
        ("$[5 -1]", plain("5"), plain("1")),
        ("$[5 - 1]", plain("5"), plain("1")),
        ("$[5-1]", plain("5"), plain("1")),
        ("$[$x -1]", Ast::Variable("x".into()), plain("1")),
        ("$[(1)-5]", plain("1"), plain("5")),
        ("$['a' -1]", Ast::Literal("a".into()), plain("1")),
        ("$[`a - 1]", nullary("a"), plain("1")),
    ] {
        assert_eq!(
            unwrap_stmts(parse(src).unwrap()),
            vec![binary(left, BinaryOp::Arith(ArithOp::Sub), right)],
            "{src:?}"
        );
    }
    assert_eq!(
        unwrap_stmts(parse("$[`a == `b]").unwrap()),
        vec![binary(nullary("a"), BinaryOp::Eq(EqOp::Eq), nullary("b"))]
    );
}

/// A sign touching no numeral stays an operator.
#[test]
fn a_sign_touching_no_numeral_is_an_operator() {
    for src in [
        "$[+ 5]",
        "$[+$x]",
        "$[+-5]",
        "$[-1e6]",
        "$[-5abc]",
        "$[-9223372036854775809 + 1]",
    ] {
        parse(src).unwrap_err();
    }
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
        keys: vec![Spanned::synthetic(IndexKey::Label("tail".into()))],
    }));
    let result = Ast::Index {
        target: Spanned::synthetic_boxed(Ast::Force(Spanned::synthetic_boxed(Ast::Block(body(
            vec![plain("cmd")],
        ))))),
        keys: vec![Spanned::synthetic(IndexKey::Label("k".into()))],
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
fn pattern_rejects_duplicate_names() {
    for src in [
        "let [x, x] = [1, 2]",
        "let [x, [_, x]] = [1, [2, 3]]",
        "let [x, ...x] = [1, 2]",
        "let [a: x, b: x] = $m",
        "echo { |[x, x]| $x }",
    ] {
        parse(src).expect_err("duplicate binding must not parse");
    }
}

#[test]
fn curried_params_may_shadow() {
    let x = || Pattern::Name("x".into());
    assert_eq!(
        unwrap_stmts(parse("echo { |x x| $x }").unwrap()),
        vec![app(
            bare_head("echo"),
            vec![arm_lambda(
                x(),
                vec![arm_lambda(x(), vec![Ast::Variable("x".into())])]
            )],
        )]
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
fn parse_map_on_written_keys_with_spread() {
    let ast = unwrap_stmts(parse("[:, \"k\": 'v', ...$d]").unwrap());
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
    assert!(parse("[:, \"k\": 1, 5]").is_err());
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
    parse("[k: 'v', ...$d]").unwrap_err();
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
            assert_eq!(&head.item, &bare_head("dispatch"));
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
    let src = "return [handlers: [:, ls: { |args| if $is-mac { echo a }\nelse { echo b } }]]";
    let ast = unwrap_stmts(parse(src).unwrap());
    let Ast::Return(Some(val)) = &ast[0] else {
        panic!("expected return");
    };
    let Ast::Record(entries) = val.item.as_ref() else {
        panic!("expected record");
    };
    let RecordEntry::Field {
        value: handlers_val,
        ..
    } = &entries[0]
    else {
        panic!("expected handlers entry");
    };
    let Ast::Map(handler_entries) = &handlers_val.item else {
        panic!("expected handlers map");
    };
    let MapEntry::Entry { value: ls_val, .. } = &handler_entries[0] else {
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
    parse("if true { echo yes } echo unexpected").unwrap_err();
}

#[test]
fn parse_if_still_allows_a_newline_separated_next_statement() {
    let ast = unwrap_stmts(parse("if true { echo yes }\necho after").unwrap());
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
        Pattern::Record(vec![FieldPattern {
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
    parse("case $r $handlers").unwrap_err();
}

#[test]
fn parse_case_rejects_a_repeated_tag() {
    parse("case $r [`ok: { |v| echo $v }, `ok: { |v| echo again }]").unwrap_err();
}

#[test]
fn parse_case_rejects_an_unbound_arm_body() {
    parse("case $r [`ok: { echo hi }]").unwrap_err();
}

#[test]
fn parse_case_rejects_a_two_parameter_arm() {
    parse("case $r [`ok: { |a b| echo hi }]").unwrap_err();
}

#[test]
fn parse_case_rejects_trailing_same_line_argument() {
    parse(&format!("{ONE_ARMED_CASE} echo unexpected")).unwrap_err();
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
fn parse_case_still_allowed_as_a_pipeline_stage() {
    let ast = unwrap_stmts(parse(&format!("{ONE_ARMED_CASE} | cat")).unwrap());
    assert_eq!(
        ast,
        vec![Ast::Pipeline(body(vec![one_armed_case(), plain("cat")]))]
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
        parse(src).expect_err("bare word after `<<` must not parse");
    }
}

/// The fd prefix is refused at the lexer, identity dups included; `2>&1`
/// binds stderr to stdout wherever it stands.
#[test]
fn fd_prefixes_name_streams() {
    for src in ["cmd 1>&1", "cmd 2>&2"] {
        parse(src).expect_err("identity dup must not parse");
    }
    let redirects = redirects_of("cmd 2>&1 > o");
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
    for src in [
        "cmd < a << #'b'#",
        "cmd > a >> b",
        "cmd 2> a 2> b",
        "cmd 2> e 2>&1",
        "cmd 2>&1 2> e",
        "cmd 2>&1 2>&1",
        "try { a } { b } > a > b",
    ] {
        parse(src).expect_err(src);
    }
}

#[test]
fn a_clash_span_covers_the_second_redirect() {
    let src = "cmd > a >> b";
    let span = parse(src).expect_err(src).span.expect("span");
    assert_eq!(&src[span.start as usize..span.end as usize], ">> b");
}

/// Distinct streams never clash, in whichever order they are written.
#[test]
fn distinct_streams_bind_in_any_order() {
    for src in ["cmd < i > o 2> e", "cmd 2> e > o < i"] {
        let r = redirects_of(src);
        assert!(
            r.stdin.is_some() && r.stdout.is_some() && r.stderr.is_some(),
            "{src:?}"
        );
    }
}

/// `<<` always feeds stdin: every fd prefix is refused, `0` with its own
/// advice.
#[test]
fn herestring_fd_prefix() {
    for src in ["cat 0<< #'x'#", "cat 2<< #'x'#"] {
        parse(src).expect_err("fd-prefixed herestring must not parse");
    }
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
                &head.item,
                &Head::TildePath(TildePath {
                    suffix: Some("/.local/bin/claude".into()),
                })
            );
        }
        _ => panic!("expected zero-arg command app, got {ast:?}"),
    }
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
            assert_eq!(&head.item, &path_head("./script"));
        }
        _ => panic!("expected zero-arg path app, got {ast:?}"),
    }
}

// ── The nesting cap ─────────────────────────────────────────────────

/// What refuses a shape past the cap: the lexer's delimiter stack, or the
/// parser's descent.
enum Guard {
    Lexer,
    Parser,
}

/// A shape's name, its guard, and its source at a given depth.
type NestingShape = (&'static str, Guard, fn(usize) -> String);

/// Each shape the cap bounds, written as source `d` levels deep.  A leaf
/// operand is a level of its own, so a shape ending in one nests that much
/// less to reach the same depth.
const NESTING_SHAPES: [NestingShape; 10] = [
    ("blocks", Guard::Parser, |d| {
        format!("{}{}", "{".repeat(d), "}".repeat(d))
    }),
    ("lists", Guard::Parser, |d| {
        format!("return {}{}", "[".repeat(d), "]".repeat(d))
    }),
    ("list patterns", Guard::Parser, |d| {
        format!("let {}a{} = x", "[".repeat(d - 1), "]".repeat(d - 1))
    }),
    ("grouped operands", Guard::Parser, |d| {
        format!("return $[{}1{}]", "(".repeat(d - 2), ")".repeat(d - 2))
    }),
    ("negations", Guard::Parser, |d| {
        format!("return $[{}1]", "- ".repeat(d - 2))
    }),
    ("`not`s", Guard::Parser, |d| {
        format!("return $[{}true]", "not ".repeat(d - 2))
    }),
    ("case arms", Guard::Parser, |d| {
        format!(
            "{}1{}",
            "case $x [`a: {|p| ".repeat(d - 1),
            "}]".repeat(d - 1)
        )
    }),
    ("expression blocks", Guard::Lexer, |d| {
        format!("return {}1{}", "$[".repeat(d), "]".repeat(d))
    }),
    ("forced strings", Guard::Lexer, |d| {
        format!("return {}x{}", "\"!{echo ".repeat(d), "}\"".repeat(d))
    }),
    ("expressions in strings", Guard::Lexer, |d| {
        format!("return {}1{}", "\"$[".repeat(d), "]\"".repeat(d))
    }),
];

/// Exactly `NESTING_DEPTH_LIMIT` levels parse and one more is refused with the
/// shared diagnostic, by the guard that owns the shape: the lexer's refusal is
/// a lex error, the parser's a plain one over a token stream the lexer accepted.
#[test]
fn the_nesting_cap_admits_its_limit_and_refuses_one_more() {
    use crate::syntax::{NESTING_DEPTH_LIMIT, nesting_too_deep_message};
    // The limit is sized for the front end's own stack, not a test thread's.
    crate::compile::on_compile_stack(|| {
        for (shape, guard, source) in NESTING_SHAPES {
            let at_limit = source(NESTING_DEPTH_LIMIT);
            assert!(parse(&at_limit).is_ok(), "{shape}: the limit itself parses");
            let past = source(NESTING_DEPTH_LIMIT + 1);
            let err = parse(&past).expect_err(shape);
            assert_eq!(err.message, nesting_too_deep_message(), "{shape}");
            match (guard, &err.kind) {
                (Guard::Lexer, ParseErrorKind::Lex(LexErrorKind::Other(_))) => {}
                (Guard::Parser, ParseErrorKind::Plain { .. }) => {
                    assert!(lexer::lex(&past).is_ok(), "{shape}: the lexer admits it");
                }
                (_, kind) => panic!("{shape}: refused by the wrong guard: {kind:?}"),
            }
        }
    });
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
fn caret_is_not_a_continuation_token() {
    assert!(!needs_continuation("^"));
}

#[test]
fn balanced_program_with_trailing_comment_does_not_need_continuation() {
    assert!(!needs_continuation("let f = { return 1 } # done"));
    assert!(!needs_continuation("echo done # done"));
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

/// `else` and `elsif` continue an `if`; as a stage head they have nothing
/// to continue.
#[test]
fn else_and_elsif_are_refused_as_stage_heads() {
    for src in [
        "else { b }",
        "elsif $c { b }",
        "echo hi\nelse { b }",
        "echo hi | elsif $c { b }",
    ] {
        parse(src).unwrap_err();
    }
}

// ── Control operators (try / guard / within / grant / audit / spawn / all)

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
        [Ast::Call { head, args }] => (
            head.item.clone(),
            args.iter().map(|s| s.item.clone()).collect(),
        ),
        _ => panic!("expected a single Ast::Call, got {stripped:?}"),
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
fn a_guard_requires_all_three_operands() {
    for source in [
        "guard",
        "guard { acquire }",
        "guard { acquire } { |r| body }",
    ] {
        parse(source).expect_err("a guard needs acquire, body and release");
    }
}

#[test]
fn guard_grant_audit_and_gather_read_their_operands() {
    let scope = |src: &str| unwrap_single_scope(parse(src).unwrap());
    let ScopeAst::Guard {
        acquire,
        body,
        release,
    } = scope("guard { acquire } { |r| body } { |r end| release }")
    else {
        panic!("expected ScopeAst::Guard");
    };
    for (operand, source) in [
        (acquire, "{ acquire }"),
        (body, "{ |r| body }"),
        (release, "{ |r end| release }"),
    ] {
        assert_eq!(strip_one(*operand), strip_one(sole_stmt(source)));
    }
    let ScopeAst::Grant { caps, body } = scope("grant [exec: [allow: []]] { body }") else {
        panic!("expected ScopeAst::Grant");
    };
    assert_eq!(caps.len(), 1);
    assert!(matches!(*body, Ast::Block(_)));
    let ScopeAst::Audit { body } = scope("audit { body }") else {
        panic!("expected ScopeAst::Audit");
    };
    assert!(matches!(*body, Ast::Block(_)));
    let ScopeAst::Gather { body } = scope("gather { body }") else {
        panic!("expected ScopeAst::Gather");
    };
    assert!(matches!(*body, Ast::Block(_)));
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
    parse("within [:] { body }").unwrap_err();
}

/// An option is named, so a computed key cannot be one.
#[test]
fn a_computed_option_key_is_refused() {
    parse("within [$k: 1] { body }").unwrap_err();
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
        parse(src).unwrap_err();
    }
}

/// Under a bracket that is no record literal, a repeated label has no
/// `DuplicateField` downstream to catch it.
#[test]
fn a_repeated_option_is_refused() {
    for src in [
        "within [dir: 'a', dir: 'b'] { body }",
        "within [handlers: [:], handlers: [:]] { body }",
        "grant [net: true, net: false] { body }",
    ] {
        parse(src).unwrap_err();
    }
}

#[test]
fn a_handler_map_stays_among_the_options() {
    let op = unwrap_single_scope(
        parse("within [handlers: [:, a: { |x| echo }], dir: '/'] { echo }").unwrap(),
    );
    let ScopeAst::Within { opts, body } = op else {
        panic!("expected ScopeAst::Within, got {op:?}");
    };
    assert_eq!(opts.len(), 2);
    assert_eq!(opts[0].0, "handlers");
    let Ast::Map(entries) = &opts[0].1.item else {
        panic!("expected handler map");
    };
    assert_eq!(entries.len(), 1);
    let MapEntry::Entry { key, value } = &entries[0] else {
        panic!("expected map entry");
    };
    assert_eq!(key.item, Ast::Literal("a".into()));
    assert!(matches!(value.item, Ast::Lambda { .. }));
    assert_eq!(opts[1].0, "dir");
    assert_eq!(opts[1].1.item, Ast::Literal("/".into()));
    assert!(matches!(*body, Ast::Block(_)));
}

#[test]
fn parse_spawn_title_and_body() {
    let (op, redirects) = redirected_scope("spawn 'sort the log' { body } > out");
    let ScopeAst::Spawn {
        title: Some(title),
        body,
    } = op
    else {
        panic!("expected a titled spawn, got {op:?}");
    };
    assert_eq!(title.item, Ast::Literal("sort the log".into()));
    assert!(title.span.is_some(), "the title keeps its span");
    assert!(matches!(*body, Ast::Block(_)));
    assert!(redirects.stdout.is_some());
}

// ── Reserved-name binding rejection ─────────────────────────────────

/// No keyword or value literal binds, whether a plain binder, a list
/// element, a rest name or a lambda parameter.
#[test]
fn a_reserved_word_is_no_binding_name() {
    use crate::syntax::{CONTROL_OPERATORS, FLOW_KEYWORDS, reserved_keyword_message};
    for word in CONTROL_OPERATORS
        .into_iter()
        .chain(FLOW_KEYWORDS)
        .chain(["true", "false"])
    {
        for src in [
            format!("let {word} = 1"),
            format!("let [a, {word}] = $x"),
            format!("let [...{word}] = $x"),
            format!("echo {{ |{word}| 1 }}"),
        ] {
            let err = parse(&src).expect_err(&src);
            assert_eq!(err.message, reserved_keyword_message(word), "{src}");
        }
    }
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
    parse("echo hi\n> out").unwrap_err();
}

// ── Targeted diagnostics ────────────────────────────────────────────

/// Every program is refused with the message and the help paired with it.
fn refused_helping(cases: &[(&str, &str, &str)]) {
    for (src, said, help) in cases {
        let err = parse(src).expect_err(src);
        assert!(
            err.message.contains(said),
            "{src:?} must say {said:?}, got: {}",
            err.message
        );
        let ParseErrorKind::Plain { hint: Some(hint) } = &err.kind else {
            panic!("{src:?} must offer help, got: {err:?}");
        };
        assert!(
            hint.contains(help),
            "{src:?} must help with {help:?}, got: {hint}"
        );
    }
}

/// The REPL asks for another line exactly when some suffix completes the
/// input, and never over a definite error that no suffix can repair.
#[test]
fn continuation_is_asked_only_where_a_suffix_completes_the_input() {
    for (prefix, suffix) in [
        ("let f = {", " return 1 }"),
        ("let f = {# comment", "\n }"),
        ("return [a, b", "]"),
        ("\"foo", "\""),
        ("\"foo\\", "\nbar\""),
        ("\"foo !{cmd", "}\""),
        ("echo $(x", ")"),
        ("$[1 +", " 2]"),
        ("let x =", " 1"),
        ("echo hi |", " cat"),
        ("echo a ?", " b"),
        ("if $c", " { a }"),
        ("if true x\nelsif", " $c { y }"),
    ] {
        assert!(needs_continuation(prefix), "{prefix:?} must ask for more");
        assert!(
            parse(&format!("{prefix}{suffix}")).is_ok(),
            "{prefix:?} must be completed by {suffix:?}"
        );
    }
    for src in ["{ ]", "[a }", "echo hi }", "echo \"\\z\"", "$[1 + ]"] {
        assert!(parse(src).is_err(), "{src:?} must not parse");
        assert!(!needs_continuation(src), "{src:?} is beyond repair");
    }
}

#[test]
fn a_rest_pattern_comes_last() {
    for src in ["let [a, ...r, b] = $x", "let [a, ...r b] = $x"] {
        parse(src).unwrap_err();
    }
    for src in ["let [a, ...r] = $x", "let [a, ...r,] = $x"] {
        assert!(parse(src).is_ok(), "{src:?} must parse");
    }
}

#[test]
fn an_entry_that_runs_a_command_says_how() {
    use EntryReading::{Binding, Command, Statement, Value};
    for (src, want) in [
        ("[k: map { |x| $x } $y]", Command),
        ("[str $x]", Command),
        ("[k: $f 1]", Command),
        ("[k: if $c { 1 } else { 2 }]", Statement("if".into())),
        ("[k: let x = 1]", Binding),
        ("[1 2]", Value),
        ("within [dir: pwd -P] { ls }", Command),
        ("[a | b]", Command),
        ("[k: $x | f]", Command),
        ("[a ? b]", Command),
        ("[a > f]", Command),
        ("[1 | 2]", Value),
    ] {
        let err = parse(src).expect_err(src);
        let ParseErrorKind::EntryRuns { reading, .. } = err.kind else {
            panic!("{src:?}: {err}");
        };
        assert_eq!(reading, want, "{src:?}");
    }
    for src in ["[if, else]", "[hostname, uname]", "[k: !{str $x}]"] {
        assert!(parse(src).is_ok(), "{src:?} must parse");
    }
}

#[test]
fn a_key_with_its_colon_glued_to_its_value_says_to_space_it() {
    refused_helping(&[
        (
            "echo [host:\"h\"]",
            "`host:` is one word",
            "for the key `host`, put a space after the colon: `host: value`",
        ),
        (
            "echo [a: 1, port:$p]",
            "`port:` is one word",
            "for the key `port`",
        ),
        (
            "within [dir:$d] { a }",
            "`dir:` is one word",
            "for the key `dir`",
        ),
    ]);
    assert!(parse("echo [http://x, y]").is_ok());
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
