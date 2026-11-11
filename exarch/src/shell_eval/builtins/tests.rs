use super::*;

/// A root over core, exarch's host surface, and the `extra` sets.
fn dressed_shell(extra: &[&'static [BuiltinEntry]]) -> Shell {
    let mut runtime = ral_core::test_helper::core_runtime();
    let surface = host_surface();
    for set in surface.statics {
        runtime.install_builtins(set);
    }
    for set in &surface.captured {
        runtime.install_captured_builtins(set);
    }
    for set in extra {
        runtime.install_builtins(set);
    }
    Shell::root(runtime.into(), ral_core::test_helper::process_cwd())
}

#[test]
fn line_hash_ignores_trailing_whitespace() {
    assert_eq!(line_hash("x"), line_hash("x   "));
}

#[test]
fn eol_round_trips_byte_for_byte() {
    for body in ["a\r\nb\r\n", "a\r\r\nb", "a\r\nb\n", "a\rb", "a\n", ""] {
        let eol = Eol::of(body.as_bytes());
        assert_eq!(eol.encode(&eol.decode(body.to_string())), body);
    }
    assert!(
        matches!(Eol::of(b"a\r\nb\n"), Eol::Lf),
        "a mixed file stays raw"
    );
}

/// A `\r\n` split across reads still counts as one CRLF ending.
#[test]
fn eol_scan_agrees_with_eol_across_chunks() {
    for body in ["a\r\nb\r\n", "\na\r\n", "a\r\nb\n", "a\r\r\nb"] {
        let bytes = body.as_bytes();
        for cut in 0..=bytes.len() {
            let mut scan = EolScan::default();
            scan.feed(&bytes[..cut]);
            scan.feed(&bytes[cut..]);
            assert_eq!(
                matches!(scan.eol(), Eol::Crlf),
                matches!(Eol::of(bytes), Eol::Crlf),
                "{body:?} cut at {cut}"
            );
        }
    }
}

/// A line may reach the limit but not pass it, whichever reads carry it.
#[test]
fn line_run_names_the_first_long_line() {
    let at = vec![b'x'; LONGEST_LINE];
    let mut run = LineRun::default();
    assert!(run.feed(b"short\n").is_ok());
    assert!(run.feed(&at).is_ok());
    assert!(run.feed(b"\nshort\nx").is_ok());
    let past = run.feed(&at).expect_err("one byte past the limit");
    assert_eq!(past.0, 4);
}

/// In a file shorter than a full floor window, every window clamps to the
/// whole file, so lines are told apart by offset alone, all at the floor.
#[test]
fn window_hashes_floor_at_min_radius() {
    let rows: Vec<String> = ["a", "b", "c", "d"]
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let hashes = window_hashes(&rows);
    assert_eq!(hashes.len(), rows.len());
    let body: String = rows.iter().map(|r| line_hash(r)).collect();
    for (i, hash) in hashes.iter().enumerate() {
        let expected = line_hash(&format!("{MIN_RADIUS}:{i}:{body}"));
        assert_eq!(*hash, expected, "row {i} at the floor radius");
    }
    let distinct: std::collections::HashSet<&String> = hashes.iter().collect();
    assert_eq!(distinct.len(), rows.len(), "all distinct");
}

/// Two identical lines sharing an offset within their floor windows are
/// separated by folded context alone — what a bare line hash cannot do.
#[test]
fn window_hashes_distinguish_repeated_lines_by_context() {
    let mut rows: Vec<String> = vec!["fn alpha() {".to_string()];
    for k in 0..5 {
        rows.push(format!("    a{k}"));
    }
    let t1 = rows.len();
    rows.push("    target".to_string());
    for k in 0..6 {
        rows.push(format!("    a{k}"));
    }
    rows.push("}".to_string());
    rows.push("fn beta() {".to_string());
    for k in 0..5 {
        rows.push(format!("    b{k}"));
    }
    let t2 = rows.len();
    rows.push("    target".to_string());
    for k in 0..6 {
        rows.push(format!("    b{k}"));
    }
    rows.push("}".to_string());

    let hashes = window_hashes(&rows);
    assert_eq!(
        line_hash(&rows[t1]),
        line_hash(&rows[t2]),
        "same line content"
    );
    assert_ne!(
        hashes[t1], hashes[t2],
        "distinct neighbourhoods must witness distinctly, even at equal offsets"
    );
}

/// What adaptive context buys over a fixed window: even a run where every
/// fixed-radius window repeats still witnesses distinctly, the interior
/// falling back to its index.
#[test]
fn window_hashes_are_unique_across_a_long_identical_run() {
    let mut rows: Vec<String> = vec!["head".to_string()];
    rows.extend((0..200).map(|_| "dup".to_string()));
    rows.push("tail".to_string());

    let hashes = window_hashes(&rows);
    let distinct: std::collections::HashSet<&String> = hashes.iter().collect();
    assert_eq!(
        distinct.len(),
        rows.len(),
        "every line, even deep in a 200-line identical run, must witness uniquely"
    );
}

/// A family builtin serialises its argument, so a block anywhere in it is
/// refused where it is written, and data of any shape is not.
#[test]
fn a_family_builtin_refuses_a_block_in_its_argument_statically() {
    let shell = dressed_shell(&[]);
    let codes = |src: &str| -> Vec<&'static str> {
        match ral_core::compile::compile_and_typecheck(
            src,
            shell.session_schemes(),
            ral_core::source::FileId::DUMMY,
            None,
            None,
        ) {
            Ok(_) => Vec::new(),
            Err(ral_core::compile::CompileError::Parse(e)) => {
                panic!("{src:?} must parse, got: {e}")
            }
            Err(ral_core::compile::CompileError::Types(errs)) => {
                errs.iter().map(|e| e.kind.code()).collect()
            }
        }
    };
    for refused in [
        "exarch-agents `reply { return 1 }",
        "exarch-agents `reply [a: [{ return 1 }]]",
        "exarch-pins `set [key: 'k', body: `card { return 1 }]",
    ] {
        assert_eq!(codes(refused), ["T0074"], "{refused}");
    }
    for admitted in [
        "exarch-agents `reply [a: [1, 2], b: `ok]",
        "exarch-pins `set [key: 'k', body: `text [spans: []]]",
    ] {
        assert!(codes(admitted).is_empty(), "{admitted}");
    }
}

/// The perimeter test's exarch half: a row whose result only its own call
/// determines is a boundary.  `surface`'s open record is an argument as
/// well as the answer, so it is no cast.
#[test]
fn a_result_only_variable_in_the_exarch_tables_is_a_boundary() {
    for entry in EXARCH_BUILTINS.iter().chain(harness::HARNESS_BUILTINS) {
        let scheme = ral_core::test_access::builtin_scheme(entry);
        assert!(
            !ral_core::test_access::has_result_only_var(scheme) || entry.decl.is_boundary(),
            "{}: a result only its own call determines, and it is no boundary",
            entry.decl.name
        );
    }
    let boundaries: Vec<&str> = EXARCH_BUILTINS
        .iter()
        .chain(harness::HARNESS_BUILTINS)
        .filter(|entry| entry.decl.is_boundary())
        .map(|entry| entry.decl.name.as_ref())
        .collect();
    assert_eq!(
        boundaries,
        ["exarch-agents", "exarch-pins", "exarch-transcript"]
    );
}

/// The generic host surface omits `detach`; exarch's engine runtime installs it.
#[test]
fn detach_is_absent_from_the_host_surface() {
    let shell = dressed_shell(&[]);
    assert!(
        shell.lookup_builtin("detach").is_none(),
        "the host surface must not carry `detach`: it cannot see the capabilities that \
         decide whether a survivor is possible at all"
    );
}

/// Exarch's engine runtime installs the verb.
#[cfg(unix)]
#[test]
fn the_recipe_gains_detach() {
    let shell = crate::boot::test_shell();
    assert!(
        shell.lookup_builtin("detach").is_some(),
        "the recipe must install the verb"
    );
}

/// `/clear` reboots through the same recipe, so the fresh engine re-gains
/// the verb.
#[cfg(unix)]
#[test]
fn clear_reboots_a_shell_that_still_carries_detach() {
    let scratch = std::sync::Arc::new(
        crate::app::Scratch::for_test(crate::app::EXARCH, "detach-clear").expect("scratch dir"),
    );
    let cwd = ral_core::host::cwd().expect("test process has a cwd");
    let engine = crate::agent::engine::Engine::root(
        &crate::INSTALLERS,
        cwd,
        ral_core::authority::policy::GrantStack::default(),
        ral_core::io::terminal::TerminalState::default(),
        scratch,
    )
    .expect("the recipe boots");

    let engine = engine.prepare_rebirth().expect("an identity root reboots");
    let names = engine
        .read(ral_core::fact::ProbeSubject::BuiltinNames, |t| {
            t.builtin_names()
        })
        .expect("an identity engine never severs");
    assert!(
        names.iter().any(|n| n == "detach"),
        "the shell `/clear` boots must carry the verb its predecessor had"
    );
}

/// Sourcing the closures and installing their docs is one act, so `help`
/// names the helpers on exactly the shells that have them.
#[test]
fn help_lists_a_library_section_only_on_a_shell_that_sourced_it() {
    use ral_core::protocol::Run;
    use ral_core::source::SourceOrigin;

    use ral_core::shell::run::{RunReport, RunRequest};

    let run_help = |shell: &mut Shell| -> String {
        let req = RunRequest::from(Run::captured("help", SourceOrigin::named("<test>")));
        match shell.run(req) {
            RunReport::Ran {
                ending, captured, ..
            } => {
                ending.into_settled().expect("`help` must run cleanly");
                String::from_utf8(
                    captured
                        .expect("Capture io yields captured bytes")
                        .stdout
                        .bytes,
                )
                .expect("help output is UTF-8")
            }
            RunReport::Static { .. } => panic!("`help` must compile"),
        }
    };

    let mut bare = ral_core::test_helper::core_shell();
    let bare_out = run_help(&mut bare);
    assert!(
        !bare_out.contains("Library:"),
        "a shell that never sourced the agent library must list no Library section, got:\n{bare_out}"
    );

    let mut dressed = crate::boot::test_shell();
    let dressed_out = run_help(&mut dressed);
    assert!(
        dressed_out.contains("Library:"),
        "an exarch-dressed shell must list a Library section, got:\n{dressed_out}"
    );
    assert!(
        dressed_out.contains("view-text-around"),
        "the Library section must name the sourced helpers, got:\n{dressed_out}"
    );
}

fn refusal<T>(result: Settled<T>) -> Error {
    match result {
        Err(ral_core::shell::runtime::Break::Error(error)) => error,
        _ => panic!("expected catchable refusal"),
    }
}

fn edit_cause(error: Error) -> EditFailure {
    let Reason::Host(host) = error.reason else {
        panic!("host refusal")
    };
    let HostFailure::Exarch(ExarchFailure::Edit(cause)) = *host else {
        panic!("edit refusal")
    };
    cause
}

#[test]
fn view_bounds_keep_shapes_ranges_and_field_paths() {
    for (value, cause) in [
        (
            Value::Int(0),
            InvalidCause::Range(NumberRange {
                value: Number::Int(0),
                minimum: Number::Int(1),
                maximum: Number::Int(i64::MAX),
            }),
        ),
        (
            Value::Bool(false),
            InvalidCause::Shape(ral_core::fact::ShapeFailure {
                expected: Expected::Shape(ValueShape::Int),
                found: ral_core::fact::Found::Bool(false),
            }),
        ),
    ] {
        assert_eq!(
            refusal(view_bound(&value, "start", "view-hash")).reason,
            Reason::Invalid(Box::new(
                InvalidFailure::new("view-hash".into(), cause).field("start".into())
            ))
        );
    }
    let mut shell = dressed_shell(&[]);
    let error = refusal(view_range(
        &[Value::string("unread"), Value::Int(2), Value::Int(1)],
        &Mooring::adrift(),
        &mut shell,
        "view-text",
    ));
    assert_eq!(
        error.reason,
        Reason::Invalid(Box::new(InvalidFailure::new(
            "view-text".into(),
            InvalidCause::HalfOpenRange(HalfOpenRange {
                start: 2usize.into(),
                end: 1usize.into()
            })
        )))
    );
}

#[test]
fn edit_hash_shape_and_field_failures_keep_the_edit_index() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("file");
    std::fs::write(&path, "body").unwrap();
    let path = Value::string(ral_core::path::shown(&path));
    let mut shell = dressed_shell(&[]);
    let error = refusal(builtin_edit_hash(
        &[path.clone(), Value::Int(3)],
        &Mooring::adrift(),
        &mut shell,
    ));
    assert_eq!(
        error.reason,
        Value::Int(3)
            .expected("edit-hash", Expected::Shape(ValueShape::List))
            .reason
    );
    assert_eq!(
        edit_cause(refusal(builtin_edit_hash(
            &[path.clone(), Value::list(vec![])],
            &Mooring::adrift(),
            &mut shell
        ))),
        EditFailure::NoEdits
    );
    for (edit, cause, field) in [
        (
            Value::Int(3),
            InvalidCause::Shape(ral_core::fact::ShapeFailure {
                expected: Expected::Record(vec!["hash".into(), "line".into()]),
                found: ral_core::fact::Found::Int(3),
            }),
            None,
        ),
        (
            Value::map(vec![]),
            InvalidCause::MissingField("hash".into()),
            None,
        ),
        (
            Value::map(vec![("hash".into(), Value::string("h1"))]),
            InvalidCause::MissingField("line".into()),
            None,
        ),
        (
            Value::map(vec![("hash".into(), Value::Bool(false))]),
            InvalidCause::Shape(ral_core::fact::ShapeFailure {
                expected: Expected::Shape(ValueShape::String),
                found: ral_core::fact::Found::Bool(false),
            }),
            Some("hash"),
        ),
        (
            Value::map(vec![
                ("hash".into(), Value::string("h1")),
                ("line".into(), Value::Int(2)),
            ]),
            InvalidCause::Shape(ral_core::fact::ShapeFailure {
                expected: Expected::Shape(ValueShape::String),
                found: ral_core::fact::Found::Int(2),
            }),
            Some("line"),
        ),
    ] {
        let error = refusal(builtin_edit_hash(
            &[path.clone(), Value::list(vec![edit])],
            &Mooring::adrift(),
            &mut shell,
        ));
        let mut expected = InvalidFailure::new("edit-hash".into(), cause);
        if let Some(field) = field {
            expected = expected.field(field.into());
        }
        assert_eq!(
            error.reason,
            Reason::Invalid(Box::new(expected.index(0usize.into())))
        );
    }
}

#[test]
fn edit_hash_absence_and_duplicate_targets_refuse_before_writing() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file");
    let path = ral_core::path::shown(&file);
    std::fs::write(&file, "body").unwrap();
    let mut shell = dressed_shell(&[]);
    let edit = |hash: &str| {
        Value::map(vec![
            ("hash".into(), Value::string(hash)),
            ("line".into(), Value::string("changed")),
        ])
    };
    let error = refusal(builtin_edit_hash(
        &[
            Value::string(path.as_str()),
            Value::list(vec![edit("stale")]),
        ],
        &Mooring::adrift(),
        &mut shell,
    ));
    assert_eq!(
        edit_cause(error),
        EditFailure::HashAbsent(EditFailureHashAbsent {
            path: path.clone(),
            hash: "stale".into()
        })
    );
    let hash = window_hashes(&rows_of("body"))[0].clone();
    let error = refusal(builtin_edit_hash(
        &[
            Value::string(path.as_str()),
            Value::list(vec![edit(&hash), edit(&hash)]),
        ],
        &Mooring::adrift(),
        &mut shell,
    ));
    assert_eq!(
        edit_cause(error),
        EditFailure::DuplicateTargetLine(EditFailureDuplicateTargetLine {
            path,
            line: 1usize.into()
        })
    );
    assert_eq!(std::fs::read_to_string(file).unwrap(), "body");
}

#[test]
fn literal_misses_keep_proven_evidence_and_distinct_match_counts() {
    for (body, from, evidence) in [
        ("a\nb", r"a\nb", LiteralMiss::Escapes),
        (
            "  indented",
            "indented",
            LiteralMiss::Indentation(1usize.into()),
        ),
        ("absent", "different", LiteralMiss::Absent),
    ] {
        let error = no_unique_match(body, from, "file", &[]);
        assert_eq!(error.hints(), []);
        assert_ne!(error.reason.advice(), Vec::<String>::new());
        assert_eq!(
            edit_cause(error),
            EditFailure::NoLiteralMatch(EditFailureNoLiteralMatch {
                path: "file".into(),
                evidence
            })
        );
    }
    let starts = ral_core::shell::builtins::strings::occurrence_starts("x x\nx", "x");
    assert_eq!(
        edit_cause(no_unique_match("x x\nx", "x", "file", &starts)),
        EditFailure::MultipleLiteralMatches(EditFailureMultipleLiteralMatches {
            path: "file".into(),
            matches: 3usize.into(),
            lines: vec![1usize.into(), 2usize.into()],
        })
    );
    let mut shell = dressed_shell(&[]);
    assert_eq!(
        edit_cause(refusal(builtin_edit_replace(
            &[
                Value::string("unread"),
                Value::string(""),
                Value::string("new")
            ],
            &Mooring::adrift(),
            &mut shell
        ))),
        EditFailure::EmptyFrom
    );
}

#[test]
fn grep_keeps_the_actual_foreign_compiler_cause() {
    let pattern = "[";
    let detail = RegexMatcherBuilder::new()
        .crlf(true)
        .build(pattern)
        .unwrap_err()
        .to_string();
    let error = refusal(search_tree(
        &mut dressed_shell(&[]),
        false,
        pattern,
        "unread",
    ));
    assert_eq!(
        error.reason,
        Reason::Invalid(Box::new(InvalidFailure::new(
            "grep-files".into(),
            InvalidCause::Codec(CodecFailure::Regex(RegexFailure {
                pattern: pattern.into(),
                cause: RegexCause::GrepCompiler(detail),
            }))
        )))
    );
}

#[test]
fn file_read_and_walk_keep_io_subjects_and_utf8_offsets() {
    let root = tempfile::tempdir().unwrap();
    let missing = ral_core::path::shown(root.path().join("missing"));
    let mut shell = dressed_shell(&[]);
    let error = refusal(walk_root(&mut shell, "find-files", &missing));
    let Reason::Io(failure) = error.reason else {
        panic!("I/O failure")
    };
    assert_eq!(failure.operation, IoOperation::Walk);
    assert_eq!(
        failure.subject,
        IoSubject::Path(quote_native(OsStr::new(&missing)))
    );
    assert!(matches!(
        *failure.cause.leaf,
        ral_core::fact::IoLeaf::Os(ral_core::fact::OsFailure {
            kind: ral_core::fact::IoKind::NotFound,
            ..
        })
    ));
    let file = root.path().join("bytes");
    std::fs::write(&file, [b'x', 0xff]).unwrap();
    let path = ral_core::path::shown(&file);
    let error = refusal(read_text_file(&mut shell, &path, "view-text"));
    assert_eq!(
        error.reason,
        Reason::Invalid(Box::new(InvalidFailure::new(
            "view-text".into(),
            InvalidCause::Codec(CodecFailure::Utf8(CodecFailureUtf8 {
                subject: Utf8Subject::Path(path),
                cause: Utf8Failure {
                    valid_up_to: 1usize.into(),
                    error_len: Some(1usize.into())
                },
            }))
        )))
    );
}

#[test]
fn explore_depth_and_skill_name_refusals_are_typed_before_inspection() {
    let mut shell = dressed_shell(&[]);
    for depth in [Value::Int(-1), Value::Unit] {
        let error = refusal(builtin_explore_dir(
            &[depth, Value::string("unread")],
            &Mooring::adrift(),
            &mut shell,
        ));
        let Reason::Invalid(failure) = error.reason else {
            panic!("invalid depth")
        };
        assert_eq!(failure.operation, "explore-dir");
        assert_eq!(
            failure.path,
            [ral_core::fact::DataStep::Field("depth".into())]
        );
        assert!(matches!(
            failure.cause,
            InvalidCause::Range(_) | InvalidCause::Shape(_)
        ));
    }
    let error = refusal(builtin_skill(
        &[Value::string("../invalid")],
        &Mooring::adrift(),
        &mut shell,
    ));
    assert_eq!(
        error.reason,
        skill::refusal(SkillFailure::InvalidName(SkillFailureInvalidName {
            name: "../invalid".into()
        }))
        .reason
    );
}

#[test]
fn edit_and_skill_facts_keep_lossless_evidence_through_datum() {
    use ral_core::data::datum::Datum;
    let errors = [
        edit_error(EditFailure::HashAmbiguous(EditFailureHashAmbiguous {
            path: "file".into(),
            hash: "h123456".into(),
            lines: vec![Count::from(u64::MAX)],
        })),
        skill::refusal(SkillFailure::Missing(SkillFailureMissing {
            name: "missing".into(),
        })),
        skill::refusal(SkillFailure::Read(ral_core::fact::SkillFailureRead {
            name: "unreadable".into(),
            place: ral_core::fact::SkillPlace::Disk("\"skills/unreadable\"".into()),
            cause: ral_core::io::failure(&std::io::ErrorKind::PermissionDenied.into()),
        })),
    ];
    for error in errors {
        let reason = error.reason;
        assert_eq!(Reason::decode(&reason.clone().encode()).unwrap(), reason);
    }
}
