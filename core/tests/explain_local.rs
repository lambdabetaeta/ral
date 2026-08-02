//! `explain <name>` for a user-defined function surfaces the checker's
//! generalised scheme straight off the local binding — the same scheme that
//! seeds the next run's check — and reports where the name lives.  A local
//! binding shadows every other resolution at runtime, so it answers first.

mod common;

use ral_core::transport::{Program, Run};
use ral_core::typecheck::fmt_scheme;
use ral_core::types::{Capabilities, Settled, Shell, Value};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, builtins};

fn shell() -> Shell {
    let mut s = Shell::default();
    builtins::register(&mut s, common::prelude_comp());
    s
}

/// One REPL run through the public `run` door, stdout captured.
fn run_capture(shell: &mut Shell, src: &str) -> (Settled<Value>, String) {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran {
            result, captured, ..
        } => {
            let stdout = captured.map(|c| c.stdout).unwrap_or_default();
            (
                result,
                String::from_utf8(stdout).expect("captured stdout is UTF-8"),
            )
        }
        RunReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
    }
}

fn run(shell: &mut Shell, src: &str) -> Settled<Value> {
    run_capture(shell, src).0
}

/// The scheme `name` carries on the live scope.
fn scheme_of(sh: &Shell, name: &str) -> Option<ral_core::typecheck::Scheme> {
    sh.binding_schemes()
        .into_iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, scheme)| scheme)
}

/// A checked top-level `let` function explains to its harvested most general
/// type and a `local` source line.
#[test]
fn explain_prints_local_bindings_generalised_scheme() {
    let mut sh = shell();
    run(&mut sh, "let idf = { |x| return $x }").unwrap();
    let expected = fmt_scheme(&scheme_of(&sh, "idf").expect("a top-level let carries a scheme"));

    let (result, out) = run_capture(&mut sh, "explain idf");
    result.unwrap();
    assert!(
        out.contains(&expected),
        "explain must print the binding's scheme {expected:?}, got:\n{out}"
    );
    assert!(
        out.contains("idf: local"),
        "explain must report the name as local, got:\n{out}"
    );
}

/// A local without a scheme — a pattern-bound name — keeps the plain
/// source-line answer rather than inventing a type.
#[test]
fn explain_scheme_less_local_falls_back_to_source_line() {
    let mut sh = shell();
    run(&mut sh, "let [pa, pb] = [1, 2]").unwrap();
    assert!(
        scheme_of(&sh, "pa").is_none(),
        "pattern binds carry no scheme"
    );

    let (result, out) = run_capture(&mut sh, "explain pa");
    result.unwrap();
    assert_eq!(
        out, "explain: pa: local\n",
        "a scheme-less local must fall back to the bare source line"
    );
}

/// A local shadowing a prelude name wins the resolution: `explain` shows the
/// local's scheme, not the shadowed prelude doc.
#[test]
fn explain_local_shadows_prelude_entry() {
    let mut sh = shell();
    run(&mut sh, "let lines = 3").unwrap();

    let (result, out) = run_capture(&mut sh, "explain lines");
    result.unwrap();
    assert!(
        out.contains("lines: local"),
        "the shadowing local must answer, got:\n{out}"
    );
    assert!(
        !out.contains("Split a string into lines"),
        "the shadowed prelude doc must not answer, got:\n{out}"
    );
}
