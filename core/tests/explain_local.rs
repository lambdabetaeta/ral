//! `explain <name>` resolves one name to what documents it, the type it
//! carries, and where the shell would find it.  A local binding shadows every
//! other resolution at runtime, so it owns the name outright; below it,
//! `explain` names the frame that would actually run — alias before handler,
//! handler before the builtin manifest.

mod common;

use ral_core::transport::{Program, Run};
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
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran {
            ending, captured, ..
        } => {
            let stdout = captured.map(|c| c.stdout).unwrap_or_default();
            (
                ending.into_result(),
                String::from_utf8(stdout).expect("captured stdout is UTF-8"),
            )
        }
        RunReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
    }
}

fn run(shell: &mut Shell, src: &str) -> Settled<Value> {
    run_capture(shell, src).0
}

/// Whether `name` carries a checker-harvested scheme on the live scope.
fn has_scheme(sh: &Shell, name: &str) -> bool {
    sh.binding_schemes()
        .into_iter()
        .any(|(n, scheme)| n == name && scheme.is_some())
}

/// A checked top-level `let` explains to the scheme the checker harvested for
/// it: the identity function's *most general* type, off the binding rather
/// than the builtin manifest.
#[test]
fn explain_prints_local_bindings_generalised_scheme() {
    let mut sh = shell();
    run(&mut sh, "let idf = { |x| return $x }").unwrap();

    let (result, out) = run_capture(&mut sh, "explain idf");
    result.unwrap();
    assert!(
        out.contains("∀α. α → Command α"),
        "explain must print the harvested scheme, got:\n{out}"
    );
    assert!(
        out.contains("idf: local"),
        "explain must report the name as local, got:\n{out}"
    );
}

/// A local owns the name it shadows even carrying no scheme — a pattern-bound
/// name — so neither the shadowed doc nor the shadowed type may answer under
/// it, which is what a registry sweep below the local would have them do.
#[test]
fn explain_scheme_less_local_inherits_nothing_from_the_shadowed_entry() {
    let mut sh = shell();
    run(&mut sh, "let [lines, rest] = [1, 2]").unwrap();
    assert!(!has_scheme(&sh, "lines"), "pattern binds carry no scheme");

    let (result, out) = run_capture(&mut sh, "explain lines");
    result.unwrap();
    assert!(
        out.contains("lines: local"),
        "the local must answer, got:\n{out}"
    );
    assert!(
        !out.contains("Split a string into lines"),
        "the shadowed prelude doc must not answer, got:\n{out}"
    );
    assert!(
        !out.contains("→"),
        "nor may the shadowed prelude's type, got:\n{out}"
    );
    assert!(
        out.contains("shadows: prelude"),
        "what the local shadows is still named, got:\n{out}"
    );
}

/// An alias installs a handler frame, so `explain` must name it as an alias
/// before the handler arm swallows it.
#[test]
fn explain_names_an_alias_before_the_handler_arm() {
    let mut sh = shell();
    run(&mut sh, "alias greet { |a| echo hi }").unwrap();

    let (result, out) = run_capture(&mut sh, "explain greet");
    result.unwrap();
    assert!(
        out.contains("greet: alias") && !out.contains("greet: handler"),
        "an alias must be named as one, got:\n{out}"
    );
}

/// A handler stacked under a native's name is what runs, so `explain` reports
/// `handler` — while the doc arm still answers off the builtin manifest.
/// Probing the manifest first would have `explain` name code that never runs.
#[test]
fn explain_prefers_a_handler_over_the_builtin_manifest() {
    let mut sh = shell();

    let (result, bare) = run_capture(&mut sh, "explain length");
    result.unwrap();
    assert!(
        bare.contains("length: builtin"),
        "undressed, `length` is a builtin, got:\n{bare}"
    );

    let (result, out) = run_capture(
        &mut sh,
        "within [handlers: [length: { |a| return 0 }]] { explain length }",
    );
    result.unwrap();
    assert!(
        out.contains("length: handler") && !out.contains("length: builtin"),
        "the handler frame is what runs, got:\n{out}"
    );
    assert!(
        out.contains("number of elements"),
        "the builtin doc still answers, got:\n{out}"
    );
}
