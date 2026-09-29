#![allow(clippy::disallowed_methods)]

//! Evaluator tests that need the Rust harness: direct `Value` and IR
//! inspection, `Shell` API configuration, timing, and `coreutils`-gated
//! pipelines. Pure-language behaviour lives in the golden scripts under
//! `tests/`; static rejections (type and parse diagnostics a script cannot
//! catch with `try`) in the `tests/reject/` corpus.

mod common;

use ral_core::builtins;
use ral_core::protocol::{Program, Run};
use ral_core::types::{Break, GrantStack, Shell, Value};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};

/// Evaluate `input` through the public run door on an already-configured
/// `shell` — parse, elaborate, typecheck against the prelude schemes, run
/// the phrases.
fn run_on(shell: &mut Shell, input: &str) -> ral_core::types::Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(input.into()),
            script_name: "<test>".into(),
            caps: GrantStack::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        // A parse or type failure is a static diagnostic, not a run
        // outcome — `must_fail` reads it the same as a runtime error, since
        // both mean "this program never produced a value."
        RunReport::Static { diagnostics } => {
            let msg = ral_core::diagnostic::format_static_diagnostics(&diagnostics).0;
            Err(Break::Error(ral_core::types::Error::new(msg, 2)))
        }
    }
}

/// [`run_on`] against a fresh shell seeded with `path` on `PATH`.
fn eval_on_path(input: &str, path: &str) -> ral_core::types::Settled<Value> {
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    shell.set_env_var("PATH", path);
    builtins::register(&mut shell, common::prelude_comp());
    run_on(&mut shell, input)
}

/// Evaluate independently of whichever tools happen to be installed on the
/// test host. Individual process tests supply an explicit path when needed.
fn eval(input: &str) -> ral_core::types::Settled<Value> {
    #[cfg(unix)]
    let path = "/bin:/usr/bin".to_string();
    #[cfg(windows)]
    let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .find(|dir| {
            ["cat.exe", "pwd.exe", "sleep.exe"]
                .iter()
                .all(|name| dir.join(name).is_file())
        })
        .unwrap_or_else(|| {
            panic!("eval_fuzz needs cat.exe, pwd.exe, and sleep.exe in one PATH directory")
        })
        .to_string_lossy()
        .into_owned();

    eval_on_path(input, &path)
}

fn must_succeed(input: &str) -> Value {
    eval(input).unwrap_or_else(|e| panic!("should succeed: {input:?}\n  error: {e:?}"))
}

fn must_fail(input: &str) {
    assert!(eval(input).is_err(), "should fail: {input:?}");
}

#[test]
fn quoted_literal_pipeline_stage_is_rejected_before_command_lookup() {
    // A literal cannot become an implicit argument to the next stage.  The
    // checker rejects the value payload before it attempts to resolve `blah`.
    must_fail("'abc' | blah");
}

#[test]
fn is_empty_on_int_is_type_error() {
    must_fail("is-empty 42");
}

#[test]
fn try_error_record_carries_the_script() {
    assert_eq!(
        must_succeed(
            "try { fail [status: 1, message: 'x'] } { |err| \
             case $err[site] [`just: { |s| return \"$s[script]:$s[line]\" }, `none: { |_| return none }] }"
        ),
        Value::string("<test>:1")
    );
}

#[test]
fn len_on_int_is_error() {
    must_fail("length 42");
}

#[test]
fn env_overrides_shadow_process_env_in_dollar_env() {
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    shell.set_env_var("RAL_TEST_ENV", "override");
    builtins::register(&mut shell, common::prelude_comp());
    let result = run_on(&mut shell, "return $ENV[RAL_TEST_ENV]").expect("evaluate $ENV");
    assert_eq!(result, Value::string("override"));
}

#[test]
fn on_exit_runs() {
    // exit always returns Err(Break::Escape(Escape::Exit(_))) — callers decide whether to treat it as clean.
    let result = eval("exit 0");
    match result {
        Err(Break::Escape(ral_core::types::Escape::Exit(0))) => {}
        other => panic!("expected exit 0, got: {other:?}"),
    }
}

/// `$STATUS` is not a name any longer: a failure carries its status in the
/// `Err` record `try` binds, so reading the register fails like any other
/// undefined name — and the hint says where the status went.
#[test]
fn status_is_an_undefined_variable_that_names_its_replacement() {
    let Err(Break::Error(e)) = eval("return $STATUS") else {
        panic!("$STATUS must not resolve");
    };
    assert_eq!(e.message, "undefined variable: $STATUS");
    let hint = e
        .hint
        .expect("the miss must say what replaced the register");
    assert!(hint.contains("try"), "hint should point at `try`: {hint}");
}

/// The PATH-shadow vet guards both session-scope binders — `eval_bind`'s
/// pattern check and `eval_letrec`'s group install — so all three spellings
/// of a `let` naming a PATH-reachable command must be refused with the same
/// diagnostic.  Inside a block frame the very same `let` is legal, which is
/// what makes the refusal a scope rule rather than a ban on the name.
#[cfg(unix)]
#[test]
fn session_scope_let_may_not_shadow_a_path_command() {
    for src in [
        "let cat = 1",
        "let cat = { |x| if $[$x <= 0] { return 0 } else { cat $[$x - 1] } }",
        "let [cat, other] = [1, 2]",
    ] {
        match eval(src) {
            Err(Break::Error(e)) => {
                assert!(
                    e.message.contains("cannot bind `cat`")
                        && e.message.contains("reachable on PATH"),
                    "wrong refusal for {src:?}: {e:?}"
                );
                assert!(
                    e.hint
                        .as_deref()
                        .is_some_and(|h| h.contains("value and command names disjoint")),
                    "refusal for {src:?} must hint at the namespace rule: {e:?}"
                );
            }
            other => panic!("session-scope {src:?} must be refused, got {other:?}"),
        }
    }
    assert_eq!(
        must_succeed("return !{ let cat = 1; return $cat }"),
        Value::Int(1)
    );
}

#[cfg(feature = "coreutils")]
#[test]
fn bundled_uutils_capture_honours_scoped_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let script = format!(
        "within [dir: '{}'] {{ let out = pwd\nreturn $out }}",
        dir.path().display()
    );
    let Value::String(output) = must_succeed(&script) else {
        panic!("expected pwd output string");
    };
    assert_eq!(
        std::fs::canonicalize(output.as_str()).unwrap(),
        dir.path().canonicalize().unwrap()
    );
}

#[cfg(feature = "coreutils")]
#[test]
fn bundled_uutils_capture_completes_with_buffer_sink() {
    let out = must_succeed("let out = ls -lah\nreturn $out");
    match out {
        Value::String(s) => assert!(!s.is_empty()),
        other => panic!("expected captured ls output, got {other:?}"),
    }
}

/// `$upper` is a plain env hit on the native — the entry *is* the value.
#[test]
fn return_deref_name_is_bound_value() {
    let v = must_succeed("return $upper");
    match v {
        Value::Native { entry, applied } => {
            assert_eq!(entry.name, "upper");
            assert!(applied.is_empty());
        }
        other => panic!("expected the native itself, got {other:?}"),
    }
}

#[test]
fn lexical_non_head_name_uses_deref_to_get_value() {
    let v = must_succeed("let f = { |x| return $x }\nreturn $f");
    assert!(matches!(v, Value::Thunk(_)), "expected thunk, got {v:?}");
}

#[test]
fn list_position_deref_gives_thunk() {
    // $upper in list position is a variable lookup.
    let v = must_succeed("let upper = { |x| return $x }\nreturn [$upper]");
    match v {
        Value::List(items) => {
            assert!(
                matches!(items.get(0).as_deref(), Some(Value::Thunk(_))),
                "expected thunk, got {:?}",
                items.get(0)
            );
        }
        other => panic!("expected list, got {other:?}"),
    }
}

#[test]
fn race_first_wins() {
    // Two spawns: one returns immediately, one sleeps. Race picks the fast one.
    assert_eq!(
        must_succeed(
            r"
            let fast = !{spawn { return winner }}
            let slow = !{spawn { sleep 10; return loser }}
            let r = race [$fast, $slow]
            return $r[value]
        "
        ),
        Value::string("winner")
    );
}

#[test]
fn race_cancelled_await() {
    // After race, awaiting the loser returns an error (cancelled).
    must_fail(
        r"
        let fast = !{spawn { return ok }}
        let slow = !{spawn { sleep 10; return late }}
        race [$fast, $slow]
        !{await $slow}
    ",
    );
}

#[test]
fn cancel_makes_await_fail() {
    must_fail(
        r"
        let h = !{spawn { sleep 1; return ok }}
        cancel $h
        !{await $h}
    ",
    );
}

#[test]
fn arith_negate_rejects_non_numeric() {
    must_fail("let xv = hello\nreturn $[-$xv]");
}

#[test]
fn interpolation_rejects_list() {
    must_fail("let xs = [1, 2]\necho \"items: $xs\"");
}

/// A bundled byte stage runs as a direct `ral --ral-bundled-tool` child
/// (no helper eval) and produces correct bytes and a success status.
/// `printf … | wc -l` spawns both `printf` and `wc` as direct bundled
/// children; capturing the count through `from-string` recovers `wc`'s
/// bytes (`"4\n"` for four lines) and proves the pipeline succeeded — a
/// non-zero `wc` would fail the `from-string` capture instead.
#[cfg(all(unix, feature = "coreutils"))]
#[test]
fn bundled_byte_stage_runs_as_direct_child_and_produces_bytes() {
    assert_eq!(
        must_succeed("!{printf 'a\\nb\\nc\\nd\\n' | wc -l | from-string}"),
        Value::string("4\n"),
        "a bundled `wc -l` over four lines must emit `4`, recovered through \
         the byte-to-value `from-string` edge"
    );
}

/// A failing bundled tool in a process-staged pipeline surfaces a
/// failure rather than swallowing the non-zero status: `wc` on a missing
/// path exits non-zero, and the pipeline must fail-fast.
#[cfg(all(unix, feature = "coreutils"))]
#[test]
fn failing_bundled_byte_stage_surfaces_failure() {
    must_fail("printf 'a\\nb\\n' | wc /nonexistent/path");
}

/// `Exec` heads carrying trailing redirects must elaborate to a plain
/// `Exec` with `redirects` populated — `CompKind::Exec` is the only
/// place redirects-on-shell-call live, and the pipeline analyser
/// relies on that exclusivity at `runtime/pipeline/resolve.rs` (its
/// External fast path destructures `CompKind::Exec` directly).
#[test]
fn elaborator_never_wraps_exec_in_redirect() {
    use ral_core::ir::CompKind;

    // Each source uses an `Exec` head (bare external name, `^name`,
    // `./path`, `~/path`) with trailing redirects of every flavour
    // (stdout, stderr, append, stdin, fd-dup).  All must elaborate to
    // `Exec` with non-empty `redirects` — never to a `Scope::Redirect`
    // wrapping an `Exec`.
    let sources = &[
        "cat < in.txt",
        "echo hi > out.txt",
        "echo hi >> out.txt",
        "cmd 2> err.txt",
        "cmd 2>&1",
        "^cat < in.txt",
        "./run.sh > out.txt",
        "~/bin/tool 2> err.log",
        "cmd < in.txt > out.txt 2>&1",
    ];

    fn walk(comp: &ral_core::ir::Comp, saw_exec_with_redirects: &mut bool) {
        match &comp.item {
            CompKind::Exec(e) if ral_core::test_access::exec_has_redirects(e) => {
                *saw_exec_with_redirects = true;
            }
            CompKind::Lam { body, .. } => walk(body, saw_exec_with_redirects),
            CompKind::Bind { comp, rest, .. } => {
                walk(comp, saw_exec_with_redirects);
                walk(rest, saw_exec_with_redirects);
            }
            CompKind::App { head, .. } => walk(head, saw_exec_with_redirects),
            CompKind::Pipeline { stages, .. } => {
                for s in stages {
                    walk(s, saw_exec_with_redirects);
                }
            }
            CompKind::If { then, else_, .. } => {
                walk(then, saw_exec_with_redirects);
                walk(else_, saw_exec_with_redirects);
            }
            _ => {}
        }
    }

    for src in sources {
        let ast =
            ral_core::syntax::parser::parse(src).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
        let top = ral_core::elaborator::elaborate(&ast, std::collections::HashSet::default(), "")
            .expect("elaborate");
        let mut saw = false;
        for phrase in &top.phrases {
            let comp: &ral_core::ir::Comp = match &phrase.item {
                ral_core::ir::Phrase::Define { comp, .. } | ral_core::ir::Phrase::Run(comp) => comp,
            };
            walk(comp, &mut saw);
        }
        assert!(
            saw,
            "test bug: {src:?} elaborated without producing an `Exec` \
             with non-empty redirects — the invariant test would pass \
             vacuously.  Adjust the source to exercise the Exec+redirect \
             elaboration path."
        );
    }
}
