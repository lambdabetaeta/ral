//! Suspended matches: the elaborations they decide, the verdicts and hints they give.

use ral_core::compile::{CompileError, compile_and_typecheck};
use ral_core::ir::{CompKind, Pattern, Phrase, Toplevel};
use ral_core::source::FileId;
use ral_core::test_access::{quantified, weak_residuals};
use ral_core::ty::Var;
use ral_core::typecheck::{Reason, TypeError, TypeErrorKind, Unit};

fn schemes() -> ral_core::SessionSchemes {
    crate::common::schemes_for(&ral_core::HostSurface::default())
}

fn checked(source: &str) -> Toplevel {
    compile_and_typecheck(
        source,
        schemes(),
        FileId::DUMMY,
        None,
        Unit::Script,
        &mut crate::common::no_modules,
    )
    .unwrap_or_else(|error| panic!("{source}: {error:?}"))
}

fn refusal(source: &str) -> Vec<TypeError> {
    match compile_and_typecheck(
        source,
        schemes(),
        FileId::DUMMY,
        None,
        Unit::Script,
        &mut crate::common::no_modules,
    ) {
        Err(CompileError::Types(errors)) => errors,
        other => panic!("{source}: expected a type refusal, got {other:?}"),
    }
}

fn refused(source: &str, code: &str, hint: Option<&str>) {
    let errors = refusal(source);
    assert_eq!(
        errors
            .iter()
            .map(|error| error.kind.code())
            .collect::<Vec<_>>(),
        [code],
        "{source}: {errors:?}"
    );
    if let Some(hint) = hint {
        assert!(
            errors[0].hint().is_some_and(|text| text.contains(hint)),
            "{source}: {errors:?}"
        );
    }
}

fn elaborations(tree: &Toplevel) -> (usize, usize) {
    let mut captures = 0;
    let mut splits = 0;
    for phrase in &tree.phrases {
        let (Phrase::Define { comp, .. } | Phrase::Run(comp)) = &phrase.item;
        crate::common::walk_comp(comp, &mut |comp| match &comp.item {
            CompKind::Capture(_) => captures += 1,
            CompKind::Bind { pattern, .. } if matches!(pattern.as_ref(), Pattern::Name(name) if name.starts_with("%split")) =>
            {
                splits += 1;
            }
            _ => {}
        });
    }
    (captures, splits)
}

fn defined_scheme<'a>(tree: &'a Toplevel, name: &str) -> &'a ral_core::ty::Closed {
    tree.phrases
        .iter()
        .find_map(|phrase| match &phrase.item {
            Phrase::Define { schemes, .. } => schemes
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, scheme)| scheme),
            Phrase::Run(_) => None,
        })
        .unwrap_or_else(|| panic!("missing scheme {name}"))
}

fn scheme(tree: &Toplevel, name: &str) -> String {
    defined_scheme(tree, name).to_string()
}

#[test]
fn weak_env_payload_does_not_prevent_independent_producer_grade_generalisation() {
    for (definition, call) in [
        ("let probe-env = { return !{env}[MARK] }", "probe-env"),
        (
            "let probe-env = { |n| if $[$n <= 0] { return !{env}[MARK] } else { probe-env $[$n - 1] } }",
            "probe-env 0",
        ),
    ] {
        let before = checked(definition);
        let after = checked(&format!("{definition}; let x = {call}"));
        for tree in [&before, &after] {
            let producer = defined_scheme(tree, "probe-env");
            let quantified = quantified(producer);
            assert!(matches!(quantified[..], [Var::Grade(_)]), "{producer:?}");
            assert!(ral_core::test_access::has_weak_residuals(producer));
            assert_eq!(elaborations(tree), (0, 0));
        }
        assert_eq!(scheme(&before, "probe-env"), scheme(&after, "probe-env"));
        assert_eq!(
            weak_residuals(defined_scheme(&after, "probe-env")),
            weak_residuals(defined_scheme(&after, "x"))
        );
    }
}

#[test]
fn weak_from_json_producer_instances_do_not_capture_a_unit_binding_at_a_command_join() {
    let definition = "let f = { return !{from-json << \"null\"} }";
    let baseline = checked(&format!("{definition}; let x = f; to-json $x"));
    assert_eq!(elaborations(&baseline), (0, 0));
    assert!(ral_core::test_access::has_weak_residuals(defined_scheme(
        &baseline, "f"
    )));
    for uses in [
        "let x = f; if true { f } else { echo ignored }; to-json $x",
        "if true { f } else { echo ignored }; let x = f; to-json $x",
    ] {
        let source = format!("{definition}; {uses}");
        let tree = checked(&source);
        assert_eq!(elaborations(&tree), (0, 0), "{source}");
        assert_eq!(scheme(&tree, "x"), "Unit", "{source}");
        let producer = defined_scheme(&tree, "f");
        let grades = quantified(producer)
            .into_iter()
            .filter(|var| matches!(var, Var::Grade(_)))
            .count();
        assert_eq!(grades, 1, "{producer:?}");
        assert!(producer.to_string().contains("Command["), "{producer}");
        assert!(producer.to_string().contains("Unit"), "{producer}");
    }
}

#[test]
fn literal_index_of_decoded_data_keeps_its_from_json_weak_note() {
    let source = include_str!(
        "../../../../../tests/reject/literal-index-of-decoded-data-is-not-generic.ral"
    );
    let errors = refusal(source);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].kind.code(), "T0010");
    let hint = errors[0].hint().expect("weak boundary guidance");
    assert!(
        hint.contains("what `from-json` reads has one type for the whole unit"),
        "{hint}"
    );
}

#[test]
fn recursive_row_source_fixture_has_a_closed_recursive_scheme() {
    for source in [
        include_str!("../../../../../tests/accept/a-record-in-its-own-field-is-productive.ral"),
        "let f = { |r| let [a: x] = $r; let [b: y] = $x; [$x, $r] }",
    ] {
        let tree = checked(source);
        let printed = scheme(&tree, "f");
        assert!(printed.contains('μ'), "{printed}");
        assert!(
            printed.contains("a:") && printed.contains("b:"),
            "{printed}"
        );
    }
}

#[test]
fn capture_transpositions_capture_in_both_orders() {
    for source in [
        "let g = { |f| let y = !$f; if true { !$f } else { echo a } }",
        "let g = { |f| if true { !$f } else { echo a }; let y = !$f; return $y }",
    ] {
        assert_eq!(elaborations(&checked(source)), (1, 0), "{source}");
    }
}

#[test]
fn application_transpositions_insert_one_split_in_both_orders() {
    for source in [
        "let g = { |f| f 1; if true { !$f } else { return { |n| echo $n } } }",
        "let g = { |f| let k = if true { !$f } else { return { |n| echo $n } }; f 1 }",
    ] {
        assert_eq!(elaborations(&checked(source)), (0, 1), "{source}");
    }
}

#[test]
fn gate_transpositions_refuse_the_final_unit_head() {
    for source in [
        "let g = { |x| echo $x; return [$x, ()] }",
        "let g = { |x| return [$x, ()]; echo $x }",
    ] {
        refused(source, "T0057", None);
    }
}

#[test]
fn default_transpositions_retain_both_conflicting_uses_and_offer_both_spellings() {
    for (source, reading) in [
        (
            "let g = { |f| let a = !$f; let b = f 1; return $b }",
            "if `f` is a function, bind it with `$f` rather than running it;",
        ),
        (
            "let g = { |f| let b = f 1; let a = !$f; return $b }",
            "if `f` is a function, bind it with `$f` rather than running it;",
        ),
        (
            "let g = { |run| let a = run; let b = run 1; return $b }",
            "if `run` is a function, bind it with `$run` rather than running it;",
        ),
        (
            "let g = { |f| let a = if true { !$f } else { !$f }; let b = f 1; return $b }",
            "if `f` is a function, bind it without running it;",
        ),
        (
            "let g = { |r| let a = !$r[k]; let b = $r[k] 1; return $b }",
            "if the computation is a function, bind it without running it;",
        ),
    ] {
        refused(source, "T0080", Some(reading));
        let errors = refusal(source);
        let TypeErrorKind::ConflictingDefaults {
            binding: Some(binding),
            call: Some(call),
            ..
        } = &errors[0].kind
        else {
            panic!("both use sites must be retained: {errors:?}");
        };
        assert_ne!(binding, call);
        let hint = errors[0].hint().expect("conflict offers both spellings");
        assert!(hint.contains("call the function it binds"), "{hint}");
    }
}

#[test]
fn explicit_default_spellings_remain_distinct_and_accepted() {
    let direct = checked("let g = { |f| let a = $f; let b = f 1; return $b }");
    let returned = checked("let g = { |f| let h = !$f; let b = h 1; return $b }");
    assert!(scheme(&direct, "g").contains("{Integer → Returns"));
    assert!(scheme(&returned, "g").contains("{Returns {Integer → Returns"));
    assert_ne!(scheme(&direct, "g"), scheme(&returned, "g"));
}

#[test]
fn app2_sibling_regions_and_argument_transpositions_capture() {
    for producer in [
        "{ let z = 1; return { echo hi } }",
        "{ return { echo hi } }",
    ] {
        for source in [
            format!(
                "let app2 = {{ |k mk| let t = !$mk; k $t }}; app2 {{ |f| let y = !$f; return $y }} {producer}"
            ),
            format!(
                "let app2 = {{ |mk k| let t = !$mk; k $t }}; app2 {producer} {{ |f| let y = !$f; return $y }}"
            ),
        ] {
            assert_eq!(elaborations(&checked(&source)), (1, 0), "{source}");
        }
    }
}

#[test]
fn old_unit_scrutinee_is_refused_at_the_binding() {
    for source in [
        "let g = { |f| if true { !$f } else { return () }; let y = !$f; y 1 }",
        "let g = { |f| let y = !$f; if true { !$f } else { return () }; y 1 }",
    ] {
        let errors = refusal(source);
        let error = errors
            .iter()
            .find(|error| {
                matches!(
                    ral_core::test_access::type_error_reason(error),
                    Some(Reason::Binding { .. })
                )
            })
            .unwrap_or_else(|| panic!("{source}: expected refusal at the binding, got {errors:?}"));
        let rhs = source.find("let y = ").unwrap() + "let y = ".len();
        assert_eq!(error.pos.unwrap().range(), rhs..rhs + "!$f".len());
    }
}

#[test]
fn unchanged_programs_and_transpositions_keep_their_verdicts() {
    refused(
        "each { |t| let y = !$t; echo $y } [{ return () }]",
        "T0057",
        None,
    );
    refused(
        "let flipped = { |ts k| each $k $ts }; flipped [{ return () }] { |t| let y = !$t; echo $y }",
        "T0057",
        None,
    );
    for source in [
        "map { |f| let y = f 1; return $y } [{ |n| echo $n }]",
        "let flipped = { |fs k| map $k $fs }; flipped [{ |n| echo $n }] { |f| let y = f 1; return $y }",
    ] {
        assert_eq!(elaborations(&checked(source)), (1, 0));
    }
    for source in [
        "let g = { |f| if true { !$f } else { return () }; let y = !$f; upper $y }",
        "let g = { |f| let y = !$f; if true { !$f } else { return () }; upper $y }",
    ] {
        refused(source, "T0010", None);
    }
}

#[test]
fn late_diagnostics_are_sorted_by_source_span() {
    let source = "let g = { |x| echo $x; return [$x, ()] }; let h = { |f| let a = !$f; let b = f 1; return $b }";
    let errors = refusal(source);
    assert_eq!(
        errors
            .iter()
            .map(|error| error.kind.code())
            .collect::<Vec<_>>(),
        ["T0057", "T0080"]
    );
    assert!(
        errors
            .windows(2)
            .all(|pair| pair[0].pos.unwrap().start <= pair[1].pos.unwrap().start)
    );
}

#[test]
fn capture_program_table_accepted_rows_have_exactly_the_written_coercions() {
    // Every accepted row of design/capture, including its "likewise" variant.
    for (source, captures) in [
        ("let h = hostname", 1),
        ("let f = { hostname }; let h = f", 1),
        ("let c = true; let x = if $c { echo a } else { warn b }", 1),
        ("try { mkdir d } { |e| warn no }", 0),
        (
            "within [handlers: [:, curl: { |a| return () }]] { let x = curl foo }",
            1,
        ),
        (
            "within [handlers: [:, curl: { |_| let x = echo hi; echo $x }]] { curl }",
            1,
        ),
        (
            "let handlers = [:, curl: { |_| let x = echo hi; echo $x }]; within [handlers: $handlers] { curl }",
            1,
        ),
        (
            "let u = 'url'; try { curl $u > out } { |e| echo failed }",
            0,
        ),
        ("try { each-line { |l| warn $l } } { |e| echo failed }", 0),
        (
            "let c = true; let u = (); if $c { return $u } else { echo a }",
            0,
        ),
        ("each { |b| !$b } [{ echo hi }, { return () }]", 0),
        ("map { |f| echo $f } [1, 2]", 0),
        ("let x = map { |f| echo $f } [1, 2]", 0),
        ("let g = { |f| echo $f }; let x = map $g [1, 2]", 0),
        ("cross [1, 3] [2] { |x y| echo \"$x$y\" }", 0),
        ("let us = ['a', 'b']; par { |u| curl $u } $us 4", 0),
    ] {
        assert_eq!(elaborations(&checked(source)).0, captures, "{source}");
    }
    for source in [
        "let x = map { |f| echo $f } [1, 2]",
        "let g = { |f| echo $f }; let x = map $g [1, 2]",
    ] {
        assert_eq!(scheme(&checked(source), "x"), "[Unit]");
    }
}

#[test]
fn capture_program_table_refused_rows_keep_join_and_argument_hints() {
    for (source, hint) in [
        ("let x = try { echo a } { |e| return 'none' }", "outcome"),
        (
            "let c = true; let x = if $c { hostname } else { return 'b' }",
            "branch",
        ),
        ("both { echo a } { echo b }", "this block is a command"),
        (
            "let c = true; let x = if $c { echo a } else { from-json < f }",
            "branch",
        ),
        (
            "let xs = [1, 2]; sort-list-by { |f| echo $f } $xs",
            "this block is a command",
        ),
        (
            "let xs = [1, 2]; group-by { |f| echo $f } $xs",
            "this block is a command",
        ),
        (
            "let xs = [1, 2]; median-by { |f| echo $f } $xs",
            "this block is a command",
        ),
        (
            "let g = { |f| let y = !$f; return $y }; g { echo hi }",
            "this block is a command",
        ),
        (
            "let c = true; let twice = { |f| let _ = !$f; !$f }; if $c { twice { return () } } else { echo a }",
            "branch",
        ),
    ] {
        let errors = refusal(source);
        let count = if source.starts_with("both ") { 2 } else { 1 };
        assert_eq!(errors.len(), count, "{source}: {errors:?}");
        for error in errors {
            assert_eq!(error.kind.code(), "T0011", "{source}: {error:?}");
            assert!(
                error.hint().is_some_and(|text| text.contains(hint)),
                "{source}: {error:?}"
            );
        }
    }
}

#[test]
fn capture_program_table_accepted_rows_write_and_return_as_documented() {
    use ral_core::protocol::Run;
    use ral_core::shell::run::RunReport;
    use ral_core::shell::runtime::Value;
    use ral_core::source::SourceOrigin;

    let units = || Value::list(vec![Value::Unit, Value::Unit]);
    // External names are handled so these examples need neither installed tools nor network.
    let cases = [
        (
            "within [handlers: [:, hostname: { |_| echo host }]] { let h = hostname; return $h }",
            "",
            "",
            Value::string("host"),
        ),
        (
            "within [handlers: [:, hostname: { |_| echo host }]] { let f = { hostname }; let h = f; return $h }",
            "",
            "",
            Value::string("host"),
        ),
        (
            "let c = true; let x = if $c { echo a } else { warn b }; return $x",
            "",
            "",
            Value::string("a"),
        ),
        (
            "within [handlers: [:, mkdir: { |_| return () }]] { try { mkdir d } { |e| warn no } }",
            "",
            "",
            Value::Unit,
        ),
        (
            "within [handlers: [:, curl: { |a| return () }]] { let x = curl foo; return $x }",
            "",
            "",
            Value::string(""),
        ),
        (
            "within [handlers: [:, curl: { |_| echo fetched }]] { let u = 'url'; try { curl $u > $out } { |e| echo failed } }",
            "",
            "",
            Value::Unit,
        ),
        (
            "try { each-line { |l| warn $l } << 'line' } { |e| echo failed }",
            "",
            "line\n",
            Value::Unit,
        ),
        (
            "let c = true; let u = (); if $c { return $u } else { echo a }",
            "",
            "",
            Value::Unit,
        ),
        (
            "each { |b| !$b } [{ echo hi }, { return () }]",
            "hi\n",
            "",
            Value::Unit,
        ),
        ("map { |f| echo $f } [1, 2]", "1\n2\n", "", units()),
        (
            "let x = map { |f| echo $f } [1, 2]; return $x",
            "1\n2\n",
            "",
            units(),
        ),
        (
            "let g = { |f| echo $f }; let x = map $g [1, 2]; return $x",
            "1\n2\n",
            "",
            units(),
        ),
        (
            "cross [1, 3] [2] { |x y| echo \"$x$y\" }",
            "12\n32\n",
            "",
            units(),
        ),
        (
            "within [handlers: [:, curl: { |a| echo ...$a }]] { let us = ['a', 'b']; par { |u| curl $u } $us 4 }",
            "a\nb\n",
            "",
            units(),
        ),
    ];
    let out_scheme = defined_scheme(&checked("let out = 'fixture'"), "out").clone();
    for (source, stdout, stderr, value) in cases {
        let mut shell = ral_core::test_helper::fresh_shell();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        shell.set_var(
            "out".into(),
            Value::string(out.to_str().expect("UTF-8 test path")),
            out_scheme.clone(),
        );
        match shell.run(Run::captured(
            source,
            SourceOrigin::named("<capture-program-table>"),
        )) {
            RunReport::Ran {
                ending, captured, ..
            } => {
                assert_eq!(
                    ending
                        .into_settled()
                        .unwrap_or_else(|error| panic!("{source}: {error:?}")),
                    value,
                    "{source}"
                );
                let captured = captured.expect("captured run");
                assert_eq!(captured.stdout.bytes, stdout.as_bytes(), "{source}");
                assert_eq!(captured.stderr.bytes, stderr.as_bytes(), "{source}");
                if source.contains("> $out") {
                    assert_eq!(std::fs::read(out).unwrap(), b"fetched\n");
                }
            }
            RunReport::Checked { .. } => {
                panic!("source fixture must run, not only check: {source}")
            }
            RunReport::Static { diagnostics, .. } => {
                panic!("{source}: {}", diagnostics.render(shell.exit_hints()).0)
            }
        }
    }
}
