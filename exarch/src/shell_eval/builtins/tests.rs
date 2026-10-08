use super::*;

/// Dress a bare test shell with exarch's host surface.
fn dress(shell: &mut Shell) {
    let surface = host_surface();
    for set in surface.statics {
        shell.install_builtins(set);
    }
    for set in &surface.captured {
        shell.install_captured_builtins(set);
    }
}

fn status(b: Break) -> i32 {
    match b {
        Break::Error(e) => e.code(),
        other @ Break::Escape(_) => panic!("expected Break::Error, got {other:?}"),
    }
}

#[test]
fn line_hash_ignores_trailing_whitespace() {
    assert_eq!(line_hash("x"), line_hash("x   "));
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

/// A pre-cancelled scope aborts `search_tree`'s walk at its first poll,
/// before any filesystem entry is touched.
#[test]
fn search_files_honours_a_cancelled_scope() {
    let mut shell = ral_core::test_helper::core_shell();
    let m = Mooring::adrift();
    m.cancel.cancel(ral_core::process::CancelCause::Interrupted);
    let err = builtin_grep_files(&[Value::string("x")], &m, &mut shell)
        .expect_err("a cancelled scope must abort the search walk");
    assert_eq!(status(err), 130);
}

#[test]
fn explore_dir_honours_a_cancelled_scope() {
    let mut shell = ral_core::test_helper::core_shell();
    let m = Mooring::adrift();
    m.cancel.cancel(ral_core::process::CancelCause::Interrupted);
    let err = builtin_explore_dir(&[Value::Int(3)], &m, &mut shell)
        .expect_err("a cancelled scope must abort the directory walk");
    assert_eq!(status(err), 130);
}

// ── `service-handle` builtin ─────────────────────────────────────────

/// The site of a `service-handle` call whose handle is used at `Int`.
fn handle_site() -> Arc<Site> {
    ral_core::test_access::site_of(&Ty::Handle(Box::new(Ty::Int)))
}

/// A worker body that blocks until cancelled, polling `Mooring::check` so the
/// thread genuinely stays `Running` rather than settling instantly.  Named
/// apart from `test-clear-block-forever` in `exarch/src/agent/testkit.rs` so
/// registering both in one test binary cannot collide.
fn builtin_test_block_forever(
    _args: &[Value],
    mooring: &Mooring,
    _shell: &mut Shell,
) -> Settled<Value> {
    loop {
        mooring.check()?;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn scheme_test_block_forever(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(pure(Ty::Unit)))
}

static WORKER_TEST_BUILTINS_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed("test-block-forever"),
    scheme_test_block_forever,
    "test-only: block until cancelled.",
    BuiltinBody::Static(builtin_test_block_forever),
)];
static WORKER_TEST_BUILTINS: &[BuiltinEntry] = &WORKER_TEST_BUILTINS_ARR;

/// Run `src` as one top-level run, deliberately without a deferred lease so
/// nothing races a reap mid-test.  Panics on any failure.
fn run_top_level(shell: &mut Shell, src: &str) {
    use ral_core::protocol::Run;

    use ral_core::run::{RunReport, RunRequest};
    let req = RunRequest::from(Run::captured(src, "<test>"));
    match shell.run(req) {
        RunReport::Ran { ending, .. } => {
            ending
                .into_result()
                .expect("worker-registry fixture source must run cleanly");
        }
        RunReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
    }
}

/// Dressed, `service` resolves to its own `Handle`-returning scheme rather
/// than falling through to an external command, so feeding it to `cancel`
/// typechecks.  The negative half of this is
/// `service_is_external_on_a_bare_core_table` in `core/tests/typecheck.rs`.
#[test]
fn service_typechecks_on_an_exarch_dressed_shell() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    match ral_core::compile::compile_and_typecheck(
        r#"let h = service "birth" { return 1 }; cancel $h"#,
        shell.session_schemes(),
        ral_core::source::FileId::DUMMY,
        "",
        None,
    ) {
        Ok(_) => {}
        Err(ral_core::compile::CompileError::Parse(e)) => {
            panic!("expected a clean parse, got: {e}")
        }
        Err(ral_core::compile::CompileError::Types(errs)) => panic!(
            "expected `service`'s Handle to satisfy `cancel` on an exarch-dressed shell, got: {:?}",
            errs.iter()
                .map(|e| e.kind.render_message())
                .collect::<Vec<_>>()
        ),
    }
}

/// A family door serialises its argument, so a block anywhere in it is
/// refused where it is written, and data of any shape is not.
#[test]
fn a_family_door_refuses_a_block_in_its_argument_statically() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    let codes = |src: &str| -> Vec<&'static str> {
        match ral_core::compile::compile_and_typecheck(
            src,
            shell.session_schemes(),
            ral_core::source::FileId::DUMMY,
            "",
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

/// The idiom `service-handle`'s doc advertises, put to the checker: two
/// call sites eliminated at different value types.  A monomorphic `Handle`
/// fails the first, an under-generalised one the second.
#[test]
fn service_handle_typechecks_under_its_documented_eliminators() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    match ral_core::compile::compile_and_typecheck(
        "let a = await !{service-handle 3}\n\
         let n = $[$a[value] + 1]\n\
         let b = await !{service-handle 4}\n\
         let s = string-replace 'x' 'y' $b[value]\n\
         cancel !{service-handle 5}",
        shell.session_schemes(),
        ral_core::source::FileId::DUMMY,
        "",
        None,
    ) {
        Ok(_) => {}
        Err(ral_core::compile::CompileError::Parse(e)) => {
            panic!("expected a clean parse, got: {e}")
        }
        Err(ral_core::compile::CompileError::Types(errs)) => panic!(
            "`service-handle`'s ∀α Handle must instantiate per call site, got: {:?}",
            errs.iter()
                .map(|e| e.kind.render_message())
                .collect::<Vec<_>>()
        ),
    }
}

#[test]
fn service_registers_as_durable_with_its_description() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    shell.install_builtins(WORKER_TEST_BUILTINS);
    run_top_level(
        &mut shell,
        r#"service "watch the thing" { test-block-forever }"#,
    );

    let entries = shell.workers();
    assert_eq!(entries.len(), 1, "exactly one registered service");
    assert_eq!(entries[0].class, ral_core::types::LeaseClass::Durable);
    assert_eq!(entries[0].cmd, "watch the thing");

    entries[0]
        .handle
        .cancel
        .cancel(ral_core::process::CancelCause::Cancelled);
}

/// The whole rediscovery idiom: birth a service without keeping its binding,
/// reacquire by id, `await` — the only way back once an eviction erases the
/// binding that named it.
#[test]
fn service_handle_reacquires_a_durable_service_and_await_round_trips() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    run_top_level(&mut shell, r#"service "answer" { 42 }"#);

    let entry = shell.workers().pop().expect("the service registered");
    assert_eq!(entry.class, ral_core::types::LeaseClass::Durable);

    #[allow(
        clippy::cast_possible_wrap,
        reason = "test WorkerId is small; no i64 wrap"
    )]
    let id = entry.id.0 as i64;
    let m = Mooring::adrift();
    let handle = match builtin_service_handle(&[Value::Int(id)], &handle_site(), &m, &mut shell) {
        Ok(Value::Handle(h)) => h,
        other => panic!("service-handle must return a Handle, got {other:?}"),
    };
    let await_fn = shell
        .lookup_builtin("await")
        .expect("core must register `await`");
    let result = await_fn
        .run(&[Value::Handle(handle)], &m, &mut shell)
        .expect("await on the reacquired handle must succeed");
    let Value::Map(record) = result else {
        panic!("await must return a record");
    };
    assert_eq!(record.get("value").as_deref(), Some(&Value::Int(42)));
}

/// A settled service nothing has claimed still lingers, a durable birth
/// arming no retention-exempting lease of its own, so `service-handle`
/// resolves it as it would a running one.
#[test]
fn service_handle_reacquires_a_settled_but_unclaimed_service() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    run_top_level(&mut shell, r#"service "answer" { 42 }"#);

    let entry = shell.workers().pop().expect("the service registered");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if entry.handle.state() != ral_core::types::HandleState::Running {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the service must settle within the budget"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(
        shell.worker_count(),
        1,
        "settled but unclaimed, the entry still lingers"
    );

    #[allow(
        clippy::cast_possible_wrap,
        reason = "test WorkerId is small; no i64 wrap"
    )]
    let id = entry.id.0 as i64;
    let m = Mooring::adrift();
    let handle = match builtin_service_handle(&[Value::Int(id)], &handle_site(), &m, &mut shell) {
        Ok(Value::Handle(h)) => h,
        other => panic!("a settled-but-retained service must still resolve, got {other:?}"),
    };
    let await_fn = shell
        .lookup_builtin("await")
        .expect("core must register `await`");
    let result = await_fn
        .run(&[Value::Handle(handle)], &m, &mut shell)
        .expect("await on a retaken, already-settled handle must deliver the cached result");
    let Value::Map(record) = result else {
        panic!("await must return a record");
    };
    assert_eq!(record.get("value").as_deref(), Some(&Value::Int(42)));
}

#[test]
fn service_handle_errors_on_an_unknown_id() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    let err = match builtin_service_handle(
        &[Value::Int(999_999)],
        &handle_site(),
        &Mooring::adrift(),
        &mut shell,
    ) {
        Err(Break::Error(e)) => e,
        other => panic!("an unknown id must error, got {other:?}"),
    };
    assert!(err.message.contains("no durable service"));
}

/// An ephemeral `spawn`'s id is refused exactly like an unknown one:
/// rediscovering an ordinary worker is the binding-lease ledger's job.
#[test]
fn service_handle_refuses_an_ephemeral_worker_id() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    shell.install_builtins(WORKER_TEST_BUILTINS);
    run_top_level(&mut shell, "spawn { test-block-forever }");

    let entry = shell.workers().pop().expect("the spawn registered");
    assert_eq!(entry.class, ral_core::types::LeaseClass::Worker);

    #[allow(
        clippy::cast_possible_wrap,
        reason = "test WorkerId is small; no i64 wrap"
    )]
    let id = entry.id.0 as i64;
    let err = match builtin_service_handle(
        &[Value::Int(id)],
        &handle_site(),
        &Mooring::adrift(),
        &mut shell,
    ) {
        Err(Break::Error(e)) => e,
        other => panic!("an ephemeral worker's id must be refused, got {other:?}"),
    };
    assert!(err.message.contains("no durable service"));

    entry
        .handle
        .cancel
        .cancel(ral_core::process::CancelCause::Cancelled);
}

/// `service-handle` is exarch's own affordance, never core's — so the REPL,
/// which installs `CORE_BUILTINS` alone, cannot reach it.
#[test]
fn service_handle_is_exarch_only_never_a_core_builtin() {
    assert!(
        EXARCH_BUILTINS
            .iter()
            .any(|e| e.decl.name.as_ref() == "service-handle"),
        "service-handle must be registered in EXARCH_BUILTINS"
    );
    assert!(
        !ral_core::builtins::CORE_BUILTINS
            .iter()
            .any(|e| e.decl.name.as_ref() == "service-handle"),
        "service-handle must never be a core builtin"
    );
}

/// Two `service-handle` calls on one worker are two sites, and each
/// `await` admits the worker's value against its own: what one script
/// decides about the answer is not what another decides.
#[test]
fn service_handle_twice_on_one_worker_admits_at_each_site() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    run_top_level(&mut shell, r#"service "answer" { 42 }"#);
    let entry = shell.workers().pop().expect("the service registered");
    #[allow(
        clippy::cast_possible_wrap,
        reason = "test WorkerId is small; no i64 wrap"
    )]
    let id = entry.id.0 as i64;
    let m = Mooring::adrift();
    let at = |ty| ral_core::test_access::site_of(&Ty::Handle(Box::new(ty)));
    let acquire =
        |shell: &mut Shell, site| match builtin_service_handle(&[Value::Int(id)], &site, &m, shell)
        {
            Ok(Value::Handle(h)) => Value::Handle(h),
            other => panic!("service-handle must return a Handle, got {other:?}"),
        };
    let as_number = acquire(&mut shell, at(Ty::Int));
    let as_text = acquire(&mut shell, at(Ty::String));
    let await_fn = shell
        .lookup_builtin("await")
        .expect("core must register `await`");
    await_fn
        .run(&[as_number], &m, &mut shell)
        .expect("the answer is a number, as the first site uses it");
    let refused = match await_fn.run(&[as_text], &m, &mut shell) {
        Err(Break::Error(e)) => e,
        other => panic!("the second site uses the answer as text, got {other:?}"),
    };
    assert!(
        refused.message.contains("is a whole number") && refused.message.contains("as text"),
        "{}",
        refused.message
    );
}

/// The perimeter test's exarch half: a row whose result only its own call
/// determines is a boundary.  `surface`'s open record is an argument as
/// well as the answer, so it is no cast.
#[test]
fn a_result_only_variable_in_the_exarch_tables_is_a_boundary() {
    for entry in EXARCH_BUILTINS.iter().chain(harness::HARNESS_BUILTINS) {
        let scheme = ral_core::test_access::builtin_scheme(entry, &mut Unifier::new());
        assert!(
            !ral_core::test_access::has_result_only_var(&scheme) || entry.decl.is_boundary(),
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
        [
            "service-handle",
            "exarch-agents",
            "exarch-pins",
            "exarch-transcript"
        ]
    );
}

/// `service`'s availability mirrors `watch`'s with the hosts swapped:
/// implemented in core but absent from both `CORE_BUILTINS` and the
/// `WATCH_BUILTIN` set the REPL adds, it reaches a shell only through
/// exarch's host surface.
#[test]
fn service_is_installed_by_exarch_and_absent_from_the_repl_sets() {
    assert!(
        ral_core::builtins::SERVICE_BUILTIN
            .iter()
            .any(|e| e.decl.name.as_ref() == "service"),
        "SERVICE_BUILTIN must carry the `service` entry"
    );
    assert!(
        !ral_core::builtins::CORE_BUILTINS
            .iter()
            .any(|e| e.decl.name.as_ref() == "service"),
        "service must never be a core builtin"
    );
    assert!(
        !ral_core::builtins::WATCH_BUILTIN
            .iter()
            .any(|e| e.decl.name.as_ref() == "service"),
        "the REPL's host surface (watch) must not smuggle service in"
    );
    let mut shell = ral_core::test_helper::core_shell();
    assert!(
        shell.lookup_builtin("service").is_none(),
        "a bare shell (the REPL's baseline) must not dispatch service"
    );
    dress(&mut shell);
    assert!(
        shell.lookup_builtin("service").is_some(),
        "exarch's host surface must install service"
    );
}

/// `service`'s story inverted: the host surface is a bare fn pointer, so a
/// `detach` it carried would arrive armed on every shell dressed through it,
/// child shells that never asked for a budget included.  The recipe
/// installs it per boot instead, in the same act that arms its policy.
#[test]
fn detach_is_absent_from_the_host_surface() {
    let mut shell = ral_core::test_helper::core_shell();
    dress(&mut shell);
    assert!(
        shell.lookup_builtin("detach").is_none(),
        "the host surface must not carry `detach`: it cannot see the capabilities that \
         decide whether a survivor is possible at all"
    );
}

/// Name and budget arrive together, so a shell that can name the verb can
/// always spend it.
#[cfg(unix)]
#[test]
fn the_recipe_gains_detach_and_its_budget() {
    let shell = crate::boot::test_shell();
    assert!(
        shell.lookup_builtin("detach").is_some(),
        "the recipe must install the verb"
    );
    assert_eq!(
        shell
            .detach_policy()
            .expect("installing the name and arming the budget is one act")
            .budget,
        crate::shell_eval::DETACH_BIRTH_BUDGET
    );
}

/// `/clear` reboots through the same recipe, so the fresh engine re-gains
/// the verb, and with it the budget the recipe arms in the same act.
#[cfg(unix)]
#[test]
fn clear_reboots_a_shell_that_still_carries_detach() {
    let dir = std::env::temp_dir().join(format!("exarch-detach-clear-{}", std::process::id()));
    let scratch = std::sync::Arc::new(
        crate::app::Scratch::for_test(crate::app::EXARCH, "detach-clear").expect("scratch dir"),
    );
    let cwd = std::env::current_dir().expect("test process has a cwd");
    let mut seat = crate::agent::seat::Seat::root(
        &crate::INSTALLERS,
        cwd,
        ral_core::terminal::TerminalState::default(),
        scratch,
        &dir,
    )
    .expect("the recipe boots");

    seat.clear().expect("an identity root reboots");
    let names = seat
        .read(|t| t.builtin_names())
        .expect("an identity seat never severs");
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

    use ral_core::run::{RunReport, RunRequest};

    let run_help = |shell: &mut Shell| -> String {
        let req = RunRequest::from(Run::captured("help", "<test>"));
        match shell.run(req) {
            RunReport::Ran {
                ending, captured, ..
            } => {
                ending.into_result().expect("`help` must run cleanly");
                String::from_utf8(captured.expect("Capture io yields captured bytes").stdout)
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

    let mut dressed = ral_core::boot::boot_shell(
        ral_core::terminal::TerminalState::default(),
        &crate::shell_eval::PRELUDE,
        &host_surface(),
    );
    crate::library::install_agent_library(&Mooring::adrift(), &mut dressed)
        .expect("embedded agent library");
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
