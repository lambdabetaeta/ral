//! Modules are closed units linked while their importer is checked.

use crate::common::top_level;

use ral_core::authority::policy::{Capabilities, FsPolicy};
use ral_core::diagnostic::Rejection;
use ral_core::fact::{Decision, Observed, Reason, Resource};
use ral_core::path::{CwdPath, FrozenPath, shown};
use ral_core::protocol::{Program, Run};
use ral_core::shell::run::{RunReport, StaticDiagnostics};
use ral_core::shell::runtime::{Break, CapturePolicy, Shell};
use ral_core::source::{SourceFile, SourceOrigin};
use ral_core::syntax::quote_word;
use ral_core::test_helper::{fresh_shell, layer};
use ral_core::{ValClos, shell::runtime::View};
use std::path::{Path, PathBuf};

fn scratch() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = ral_core::test_helper::real(tmp.path())
        .as_path()
        .to_path_buf();
    (tmp, root)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/modules")
        .join(name)
}

fn use_module(path: &Path) -> String {
    format!("use {}", quote_word(&shown(path)))
}

fn answer_of(module: &ValClos) -> i64 {
    let View::Map(record) = module.view() else {
        panic!("expected a module record, got {module:?}");
    };
    match record.get("answer").as_deref().map(ValClos::view) {
        Some(View::Int(answer)) => answer,
        other => panic!("expected an integer answer, got {other:?}"),
    }
}

#[track_caller]
fn refused(shell: &mut Shell, program: impl Into<Program>) -> Rejection {
    match shell.run(Run::captured(program, SourceOrigin::named("<importer>"))) {
        RunReport::Static {
            diagnostics: StaticDiagnostics::Compile(rejection),
            ..
        } => rejection,
        RunReport::Static {
            diagnostics: StaticDiagnostics::Host(error),
            ..
        } => panic!("expected a compile refusal, got {error}"),
        RunReport::Checked { .. } => panic!("the importer must not check"),
        RunReport::Ran { .. } => panic!("the importer must not reach evaluation"),
    }
}

#[test]
fn a_polymorphic_export_is_instantiated_at_each_call() {
    let source = format!(
        "let m = {}; let n = $m[id] 1; let s = $m[id] 'text'; return [integer: $n, text: $s]",
        use_module(&fixture("decided.ral"))
    );
    let value = top_level(&mut fresh_shell(), &source).expect("both instantiations are valid");
    assert_eq!(
        value,
        ValClos::map(vec![
            ("integer".into(), ValClos::int(1)),
            ("text".into(), ValClos::string("text")),
        ])
    );
}

#[test]
fn absent_and_misused_public_members_are_static_errors() {
    let mut shell = fresh_shell();
    for (tail, words) in [
        ("return $m[absent]", &["absent"][..]),
        ("$m[inc] 'text'", &["Integer", "String"][..]),
    ] {
        let source = format!("let m = {}; {tail}", use_module(&fixture("decided.ral")));
        let rejection = refused(&mut shell, source);
        assert!(rejection.nested.is_none(), "the module itself checks");
        let rendered = rejection.render();
        for &word in words {
            assert!(rendered.contains(word), "{rendered}");
        }
    }
}

#[test]
fn private_bindings_are_available_inside_the_module_but_not_exported() {
    let load = use_module(&fixture("decided.ral"));
    let mut shell = fresh_shell();
    let module = top_level(&mut shell, &load).expect("private helpers stay in their unit");
    assert_eq!(answer_of(&module), 42);
    let View::Map(record) = module.view() else {
        panic!("expected a module record");
    };
    assert!(record.get("_private").is_none());
    assert!(record.get("_decode").is_none());

    let rejection = refused(&mut shell, format!("let m = {load}; return $m[_private]"));
    assert!(rejection.nested.is_none());
    assert!(rejection.render().contains("_private"));
}

#[test]
fn undecided_function_and_data_exports_cite_the_boundary_in_the_module() {
    for (name, export) in [
        ("undecided-function.ral", "decode"),
        ("undecided-data.ral", "config"),
    ] {
        let module = fixture(name);
        let rejection = refused(&mut fresh_shell(), use_module(&module));
        let nested = rejection.nested.as_deref().expect("the module is refused");
        assert_eq!(nested.source.origin().file(), Some(module.as_path()));
        let [report] = nested.reports.as_slice() else {
            panic!("one undecided export must produce one diagnostic");
        };
        assert!(report.message.contains(export), "{report:?}");
        assert!(report.message.contains("undecided"), "{report:?}");
        assert!(
            report.message.contains('_'),
            "weak variables retain their marker: {report:?}"
        );
        let boundary = report
            .also
            .as_ref()
            .and_then(|label| label.span)
            .expect("the refusal points back to the boundary call");
        let text = nested.source.as_str();
        assert!(text[boundary.range()].contains("from-json"), "{report:?}");
        assert_eq!(
            text[..boundary.start as usize]
                .bytes()
                .filter(|b| *b == b'\n')
                .count(),
            2
        );
    }
}

#[test]
fn a_shadowed_undecided_definition_is_not_a_public_export() {
    let (_tmp, root) = scratch();
    let module = root.join("shadowed.ral");
    std::fs::write(
        &module,
        "let answer = echo 3 | from-json\nlet answer = 42\n",
    )
    .unwrap();
    let source = format!("let m = {}; return $[$m[answer] + 1]", use_module(&module));
    let value = top_level(&mut fresh_shell(), &source)
        .expect("only the last binding is exported, both in the checker and at runtime");
    assert_eq!(value, ValClos::int(43));
}

#[test]
fn a_final_undecided_definition_is_refused_at_the_winning_binding() {
    let (_tmp, root) = scratch();
    let module = root.join("undecided-winner.ral");
    let source = "let answer = 42\nlet answer = echo 3 | from-json\n";
    std::fs::write(&module, source).unwrap();
    let rejection = refused(&mut fresh_shell(), use_module(&module));
    let nested = rejection.nested.as_deref().expect("the module is refused");
    let [report] = nested.reports.as_slice() else {
        panic!("only the winning definition is judged");
    };
    let definition = report.at.as_ref().and_then(|label| label.span).unwrap();
    assert!(source[definition.range()].contains("from-json"));
    assert!(report.message.contains("answer"), "{report:?}");
    assert!(report.message.contains("_α"), "{report:?}");
    let boundary = report.also.as_ref().and_then(|label| label.span).unwrap();
    assert!(source[boundary.range()].contains("from-json"));
}

#[test]
fn a_closure_keeps_the_checked_type_and_value_of_an_earlier_shadowed_binding() {
    let (_tmp, root) = scratch();
    let module = root.join("captured-shadow.ral");
    std::fs::write(
        &module,
        "let config = echo 40 | from-json\n\
         let captured = { return $[$config + 2] }\n\
         let config = 'replacement'\n\
         let answer = captured\n",
    )
    .unwrap();
    let source = format!(
        "let m = {}; return [answer: $m[answer], config: $m[config]]",
        use_module(&module)
    );
    let value = top_level(&mut fresh_shell(), &source)
        .expect("shadowing changes the export, not the closure's captured value or its type");
    assert_eq!(
        value,
        ValClos::map(vec![
            ("answer".into(), ValClos::int(42)),
            ("config".into(), ValClos::string("replacement")),
        ])
    );
}

#[test]
fn an_exported_closure_cannot_hide_an_undecided_shadowed_capture() {
    let (_tmp, root) = scratch();
    let module = root.join("undecided-capture.ral");
    std::fs::write(
        &module,
        "let config = echo 40 | from-json\n\
         let captured = { return $config }\n\
         let config = 'replacement'\n",
    )
    .unwrap();
    let rejection = refused(&mut fresh_shell(), use_module(&module));
    let nested = rejection
        .nested
        .as_deref()
        .expect("the captured shape is undecided");
    let [report] = nested.reports.as_slice() else {
        panic!("only the closure remains an undecided public export");
    };
    let definition = report.at.as_ref().and_then(|label| label.span).unwrap();
    assert!(nested.source.as_str()[definition.range()].contains("let captured"));
    assert!(report.message.contains("_α"), "{report:?}");
    let boundary = report.also.as_ref().and_then(|label| label.span).unwrap();
    assert!(nested.source.as_str()[boundary.range()].contains("from-json"));
}

#[test]
fn modules_see_neither_session_bindings_nor_the_importers_lets() {
    let (_tmp, root) = scratch();
    let module = root.join("reads-caller.ral");
    std::fs::write(&module, "let answer = $caller\n").unwrap();
    for in_session in [false, true] {
        let mut shell = fresh_shell();
        let prefix = if in_session {
            top_level(&mut shell, "let caller = 41").expect("seed a checked session binding");
            ""
        } else {
            "let caller = 42;"
        };
        let rejection = refused(&mut shell, format!("{prefix} {}", use_module(&module)));
        let nested = rejection
            .nested
            .as_deref()
            .expect("the module cannot resolve caller");
        assert_eq!(nested.source.origin().file(), Some(module.as_path()));
        assert!(nested.render().contains("caller"));
    }
}

#[test]
fn module_evaluation_uses_sigma_not_the_callers_shadow_of_the_prelude() {
    let mut shell = fresh_shell();
    top_level(&mut shell, "let identity = { |value| return 7 }")
        .expect("the session can shadow a prelude function");
    let module = top_level(&mut shell, &use_module(&fixture("decided.ral")))
        .expect("the module runs under the same sigma it checked against");
    assert_eq!(answer_of(&module), 42);
}

#[test]
fn use_accepts_only_a_written_path() {
    let mut shell = fresh_shell();
    for path in ["$path", "\"$path\"", "!{ return $path }"] {
        let source = format!(
            "let path = {}; use {path}",
            quote_word(&shown(fixture("decided.ral")))
        );
        let rendered = refused(&mut shell, source).render();
        assert!(rendered.contains("written path"), "{rendered}");
    }
}

#[test]
fn fileless_written_paths_use_the_check_time_cwd() {
    let (_tmp, root) = scratch();
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(root.join("m.ral"), "let answer = 42\n").unwrap();
    std::fs::write(elsewhere.join("m.ral"), "let answer = 7\n").unwrap();
    let mut shell = fresh_shell();
    shell.seed_cwd(CwdPath::new(root).unwrap());
    for path in ["m.ral", "'m.ral'", "\"m.ral\""] {
        let source = format!(
            "within [dir: {}] {{ use {path} }}",
            quote_word(&shown(&elsewhere))
        );
        let module = top_level(&mut shell, &source).expect("a lexical dir does not relink use");
        assert_eq!(answer_of(&module), 42);
    }
    let source = format!("cd {}; use 'm.ral'", quote_word(&shown(&elsewhere)));
    let module = top_level(&mut shell, &source).expect("an earlier cd does not relink use");
    assert_eq!(answer_of(&module), 42);
}

#[test]
fn each_relative_path_is_anchored_at_its_containing_file() {
    let (_tmp, root) = scratch();
    let library = root.join("lib");
    std::fs::create_dir(&library).unwrap();
    std::fs::write(root.join("inner.ral"), "let answer = 7\n").unwrap();
    std::fs::write(library.join("inner.ral"), "let answer = 42\n").unwrap();
    std::fs::write(
        library.join("outer.ral"),
        "let inner = use 'inner.ral'\nlet answer = $inner[answer]\n",
    )
    .unwrap();
    let source = "let m = use 'lib/outer.ral'; return $m[answer]";
    let importer = root.join("main.ral");
    std::fs::write(&importer, source).unwrap();
    let value = fresh_shell()
        .run(Run::captured(
            source,
            SourceOrigin::File(SourceFile::new(importer).unwrap()),
        ))
        .expect_ran()
        .into_settled()
        .expect("the nested import is relative to outer.ral");
    assert_eq!(value, ValClos::int(42));
}

#[test]
fn use_refusals_are_not_catchable_even_in_unexecuted_code() {
    let (_tmp, root) = scratch();
    let missing = use_module(&root.join("missing.ral"));
    for source in [
        format!("try {{ {missing} }} {{ |_| return [:] }}"),
        format!("if false {{ {missing} }}"),
        format!("fail 'evaluation must not start'; {missing}"),
    ] {
        let rendered = refused(&mut fresh_shell(), source).render();
        assert!(rendered.contains("missing.ral"), "{rendered}");
    }
}

#[test]
fn unreadable_and_malformed_modules_are_compile_errors() {
    let (_tmp, root) = scratch();
    let directory = root.join("directory.ral");
    let unparsed = root.join("unparsed.ral");
    let ill_typed = root.join("ill-typed.ral");
    let non_utf8 = root.join("non-utf8.ral");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(&unparsed, "$[").unwrap();
    std::fs::write(&ill_typed, "let answer = $[1 + true]").unwrap();
    std::fs::write(&non_utf8, [0xff]).unwrap();
    for module in [&directory, &unparsed, &ill_typed, &non_utf8] {
        let rendered = refused(&mut fresh_shell(), use_module(module)).render();
        assert!(rendered.contains(&shown(module)), "{rendered}");
    }
}

#[test]
fn check_links_modules_without_evaluating_either_unit_or_installing_bindings() {
    let (_tmp, root) = scratch();
    let module = root.join("not-run.ral");
    std::fs::write(&module, "let answer = 42\nfail 'module must not run'\n").unwrap();
    let source = format!(
        "let loaded = {}; fail 'importer must not run'",
        use_module(&module)
    );
    let mut shell = fresh_shell();
    let report = shell.run(Run::captured(
        Program::Check(source),
        SourceOrigin::named("<importer>"),
    ));
    assert!(matches!(report, RunReport::Checked { .. }));
    assert!(shell.scope_lookup("loaded").is_none());

    std::fs::write(&module, "let answer = $[1 + true]\n").unwrap();
    let rendered = refused(&mut shell, Program::Check(use_module(&module))).render();
    assert!(rendered.contains(&shown(&module)), "{rendered}");
}

#[test]
fn separate_checks_reload_modules_but_a_checked_thunk_keeps_its_linked_ir() {
    let (_tmp, root) = scratch();
    let module = root.join("reload.ral");
    std::fs::write(&module, "let answer = 1\n").unwrap();
    let load = use_module(&module);
    let mut shell = fresh_shell();
    top_level(&mut shell, &format!("let load = {{ {load} }}")).expect("link the thunk's module");

    std::fs::write(&module, "let answer = 'new'\n").unwrap();
    let old = top_level(&mut shell, "load").expect("a checked thunk does not reread its module");
    assert_eq!(answer_of(&old), 1);
    let new = top_level(&mut shell, &format!("let m = {load}; upper $m[answer]"))
        .expect("a separate check reads the new export and its new type");
    assert_eq!(new, ValClos::string("NEW"));

    std::fs::write(&module, "let =").unwrap();
    let old = top_level(&mut shell, "load").expect("even invalid disk contents do not relink IR");
    assert_eq!(answer_of(&old), 1);
    refused(&mut shell, load);
}

#[test]
fn the_session_grant_judges_module_reads_before_lexical_grants_run() {
    let (_tmp, root) = scratch();
    let module = root.join("m.ral");
    std::fs::write(&module, "let answer = 42\n").unwrap();
    let source = format!("grant [fs: [read: []]] {{ {} }}", use_module(&module));
    let mut shell = fresh_shell();
    let allowed = layer(Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::freeze(&root).unwrap()],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    });
    let value = shell.with_layer(allowed, |shell| top_level(shell, &source));
    assert_eq!(
        answer_of(&value.expect("the module read precedes the lexical grant")),
        42
    );

    let denied = layer(Capabilities {
        fs: Some(FsPolicy::default()),
        ..Capabilities::default()
    });
    let source = format!(
        "grant [fs: [read: [`absolute {}]]] {{ {} }}",
        quote_word(&shown(&root)),
        use_module(&module)
    );
    let rejection = shell.with_layer(denied, |shell| refused(shell, source));
    let rendered = rejection.render();
    assert!(
        rendered.contains("grant does not allow read access"),
        "{rendered}"
    );
}

#[test]
fn a_session_read_refusal_does_not_reveal_whether_the_module_exists() {
    let (_tmp, root) = scratch();
    let module = root.join("withheld.ral");
    let load = use_module(&module);
    let denied = layer(Capabilities {
        fs: Some(FsPolicy::default()),
        ..Capabilities::default()
    });
    let mut shell = fresh_shell();
    let absent = shell.with_layer(denied.clone(), |shell| refused(shell, load.clone()));
    std::fs::write(&module, "let = 'this must not be read'").unwrap();
    let present = shell.with_layer(denied, |shell| refused(shell, load));
    assert_eq!(absent.render(), present.render());
    assert!(
        present.nested.is_none(),
        "the withheld module was never checked"
    );
}

#[test]
fn check_time_read_refusals_survive_in_the_trail_at_the_responsible_use() {
    let (_tmp, root) = scratch();
    let inner = root.join("inner.ral");
    let outer = root.join("outer.ral");
    std::fs::write(&inner, "let answer = 42\n").unwrap();
    std::fs::write(&outer, "# nested load\n\nlet m = use 'inner.ral'\n").unwrap();
    let denied = layer(Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::freeze(&root).unwrap()],
            deny_paths: vec![FrozenPath::freeze(&inner).unwrap()],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    });
    let mut shell = fresh_shell();
    for (module, script, line, col) in [
        (&inner, "<importer>".to_owned(), 2, 5),
        (&outer, shown(&outer), 3, 13),
    ] {
        let source = format!("# importer\n{}", use_module(module));
        for program in [Program::Source(source.clone()), Program::Check(source)] {
            let run = Run {
                trail: Some(CapturePolicy::Off),
                ..Run::captured(program, SourceOrigin::named("<importer>"))
            };
            let report = shell.with_layer(denied.clone(), |shell| shell.run(run));
            let RunReport::Static { diagnostics, trail } = report else {
                panic!("the session grant must refuse the check");
            };
            let rendered = diagnostics.render(shell.exit_hints()).0;
            assert!(rendered.contains(&shown(&inner)), "{rendered}");
            let [observation] = trail.as_slice() else {
                panic!("one withheld read must produce one observation: {trail:?}");
            };
            assert_eq!(observation.site.script.as_ref(), script.as_str());
            assert_eq!((observation.site.line, observation.site.col), (line, col));
            let Observed::Check(check) = &observation.what else {
                panic!("expected the check-time fs refusal: {observation:?}");
            };
            assert_eq!(check.resource, Resource::Fs);
            assert_eq!(check.decision(), Decision::Denied);
            assert_eq!(check.fields["op"], "read");
            assert_eq!(check.fields["path"], shown(&inner));
        }
    }
}

#[test]
fn the_lexical_grant_still_governs_what_a_linked_module_does() {
    let (_tmp, root) = scratch();
    let data = root.join("data");
    let module = root.join("reads-data.ral");
    std::fs::write(&data, "secret").unwrap();
    std::fs::write(
        &module,
        format!("let text = from-string < {}\n", quote_word(&shown(&data))),
    )
    .unwrap();
    let source = format!("grant [fs: [read: []]] {{ {} }}", use_module(&module));
    let Break::Error(error) =
        top_level(&mut fresh_shell(), &source).expect_err("the data read is withheld")
    else {
        panic!("expected a runtime filesystem refusal");
    };
    assert!(matches!(error.reason, Reason::Denied(_)));
}

#[test]
fn cycles_and_depth_limits_are_compile_errors_with_the_load_chain() {
    let (_tmp, root) = scratch();
    let a = root.join("a.ral");
    let b = root.join("b.ral");
    std::fs::write(&a, "let b = use 'b.ral'\n").unwrap();
    std::fs::write(&b, "let a = use 'a.ral'\n").unwrap();
    let rendered = refused(&mut fresh_shell(), Program::Check(use_module(&a))).render();
    assert!(rendered.contains("cycle"), "{rendered}");
    assert!(rendered.contains(&shown(&a)), "{rendered}");
    assert!(rendered.contains(&shown(&b)), "{rendered}");

    for index in 0..=100 {
        let source = if index == 100 {
            "let answer = 42\n".to_owned()
        } else {
            format!("let next = use '{}.ral'\n", index + 1)
        };
        std::fs::write(root.join(format!("{index}.ral")), source).unwrap();
    }
    let rendered = refused(
        &mut fresh_shell(),
        Program::Check(use_module(&root.join("0.ral"))),
    )
    .render();
    assert!(rendered.contains("depth"), "{rendered}");
    assert!(rendered.contains(&shown(root.join("0.ral"))), "{rendered}");
    assert!(
        rendered.contains(&shown(root.join("100.ral"))),
        "{rendered}"
    );
}

#[test]
fn closed_export_schemes_refuse_incompatible_handlers_statically() {
    let (_tmp, root) = scratch();
    let module = root.join("handlers.ral");
    let relay = root.join("relay.ral");
    std::fs::write(
        &relay,
        "let imported = use 'handlers.ral'\nlet bundle = $imported[bundle]\n",
    )
    .unwrap();
    for (arm, option) in [
        ("{ |args| return 7 }", "handlers: [:, foo: $m[bundle][arm]]"),
        ("{ echo handled }", "handlers: [:, foo: $m[bundle][arm]]"),
        (
            "{ |args extra| echo handled }",
            "handlers: [:, foo: $m[bundle][arm]]",
        ),
        ("{ |name args| return 7 }", "handler: $m[bundle][arm]"),
        ("{ |args| echo handled }", "handler: $m[bundle][arm]"),
        (
            "{ |name args extra| echo handled }",
            "handler: $m[bundle][arm]",
        ),
    ] {
        std::fs::write(&module, format!("let bundle = [arm: {arm}]\n")).unwrap();
        for imported in [&module, &relay] {
            let source = format!(
                "let m = {}; within [{option}] {{ foo }}",
                use_module(imported)
            );
            let rejection = refused(&mut fresh_shell(), Program::Check(source));
            assert!(
                rejection.nested.is_none(),
                "the export is decided, but is not a handler: {}",
                rejection.render()
            );
        }
    }
}

#[test]
fn closed_command_handlers_remain_callable_through_a_reexport() {
    let (_tmp, root) = scratch();
    let module = root.join("handlers.ral");
    let relay = root.join("relay.ral");
    std::fs::write(
        &relay,
        "let imported = use 'handlers.ral'\nlet bundle = $imported[bundle]\n",
    )
    .unwrap();
    for (arm, option, expected) in [
        (
            "{ |args| echo handled }",
            "handlers: [:, foo: $m[bundle][arm]]",
            "handled",
        ),
        (
            "{ |name args| echo $name }",
            "handler: $m[bundle][arm]",
            "foo",
        ),
    ] {
        std::fs::write(&module, format!("let bundle = [arm: {arm}]\n")).unwrap();
        for imported in [&module, &relay] {
            let source = format!(
                "let m = {}; let text = within [{option}] {{ foo }}; return $text",
                use_module(imported)
            );
            let value =
                top_level(&mut fresh_shell(), &source).expect("the export is a command handler");
            assert_eq!(value, ValClos::string(expected));
        }
    }
}

#[test]
fn an_exported_handler_map_is_checked_as_a_whole_when_bound() {
    let (_tmp, root) = scratch();
    let module = root.join("handler-map.ral");
    let source = format!(
        "let m = {}; within [handlers: $m[handlers]] {{ foo }}",
        use_module(&module)
    );
    std::fs::write(&module, "let handlers = [:, foo: { |args| return 7 }]\n").unwrap();
    let rejection = refused(&mut fresh_shell(), Program::Check(source.clone()));
    assert!(
        rejection.nested.is_none(),
        "the decided export is not a command map"
    );

    std::fs::write(
        &module,
        "let handlers = [:, foo: { |args| echo handled }]\n",
    )
    .unwrap();
    let RunReport::Ran {
        ending, captured, ..
    } = fresh_shell().run(Run::captured(source, SourceOrigin::named("<importer>")))
    else {
        panic!("the complete map contains commands");
    };
    assert_eq!(
        ending.into_settled().expect("the handler runs"),
        ValClos::UNIT
    );
    let captured = captured.expect("the run captures the handler's output");
    assert_eq!(captured.stdout.bytes, b"handled\n");
    assert_eq!(captured.stderr.bytes, b"");
}

#[test]
fn a_module_raising_use_prefixed_text_still_gets_loader_context_once() {
    let (_tmp, root) = scratch();
    let inner = root.join("inner.ral");
    let outer = root.join("outer.ral");
    std::fs::write(&inner, "fail 'use: user text'").unwrap();
    std::fs::write(&outer, "use 'inner.ral'").unwrap();
    let mut shell = fresh_shell();
    for module in [&inner, &outer] {
        let source = use_module(module);
        let Break::Error(error) = top_level(&mut shell, &source).expect_err("module fails") else {
            panic!("expected a module error");
        };
        assert_eq!(error.context(), Some("use"));
        assert_eq!(error.reason, Reason::raised("use: user text"));
        let record = error.fact(&shell);
        assert_eq!(record.reason(), &error.reason);
        assert_eq!(record.frame(), "fail");
        assert_eq!(record.site().script.as_ref(), shown(&inner));
    }
}
