//! The ADR session-scheme-continuity Verify list, exercised end-to-end.
//!
//! Each run's static check is seeded from the live session: the schemes
//! the checker inferred for run *N*'s top-level binds live on the runtime
//! bindings (and the alias arms' schemes on the persistent handler frames),
//! so run *N+1*'s check sees them.  The harness mirrors the REPL loop in
//! `ral/src/repl/exec.rs`: `check` seeds `compile_and_typecheck` from the
//! live `session_schemes()`, and `run` drives the public `run` door.

mod common;

use ral_core::source::FileId;
use ral_core::transport::{Program, Run};
use ral_core::types::{Capabilities, Settled};
use ral_core::{
    CompileOutcome, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Shell,
    TypeError, Value, builtins, compile_and_typecheck,
    typecheck::{TypeErrorKind, fmt_scheme},
};

fn shell() -> Shell {
    let mut s = Shell::default();
    builtins::register(&mut s, common::prelude_comp());
    s
}

/// The scheme `name` carries on the live scope, `None` when it is unbound
/// or bound without one.
fn scheme_of(sh: &Shell, name: &str) -> Option<ral_core::typecheck::Scheme> {
    sh.binding_schemes()
        .into_iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, scheme)| scheme)
}

/// One REPL run through the public `run` door, which checks `src`
/// against the live session before evaluating it.  Panics on parse / type
/// failure — callers that expect a clean run pick source that compiles;
/// callers probing an *eval* failure get the body's `Settled` back.
fn run(shell: &mut Shell, src: &str) -> Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(src.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { result, .. } => result,
        RunReport::Static { .. } => panic!("well-formed source must run: {src:?}"),
    }
}

/// Check `src` against the live session without evaluating it — the
/// errors a run would surface before running.
fn check_errors(shell: &Shell, src: &str) -> Vec<TypeError> {
    match compile_and_typecheck(src, shell.session_schemes(), FileId::DUMMY, "") {
        CompileOutcome::Compiled(_) => Vec::new(),
        CompileOutcome::Parse(e) => panic!("parse: {src:?}: {e}"),
        CompileOutcome::Types(errs) => errs,
    }
}

fn is_mode_mismatch(errs: &[TypeError]) -> bool {
    errs.iter()
        .any(|e| matches!(e.kind, TypeErrorKind::ModeMismatch { .. }))
}

// ─── (1) value producer into byte decoder is a static mode mismatch ──────────

/// `let f = { return 3 }` then `$f | from-json`: the value producer's
/// `∅` output feeding `from-json`'s ground `Bytes` input is the
/// `∅`-into-`Bytes` edge.  The next run's check reports it (T0012)
/// before evaluation, with the binding's session scheme as the seed.
#[test]
fn value_producer_into_decoder_is_static_mode_mismatch_cross_run() {
    let mut sh = shell();
    run(&mut sh, "let f = { return 3 }").unwrap();
    let errs = check_errors(&sh, "$f | from-json");
    assert!(
        is_mode_mismatch(&errs),
        "expected a cross-run ModeMismatch (T0012), got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    assert!(
        errs.iter()
            .any(|e| e.kind.render_message().contains("pipeline channels")),
        "expected the mode-mismatch message to mention pipeline channels, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

/// The same edge in a single program now reports statically too — the
/// Part-1 rule: a stage whose input resolves to ground `Bytes` takes the
/// channel edge regardless of how polymorphic its return value is.
#[test]
fn value_producer_into_decoder_is_static_mode_mismatch_in_run() {
    let sh = shell();
    let errs = check_errors(&sh, "let f = { return 3 }\n$f | from-json");
    assert!(
        is_mode_mismatch(&errs),
        "expected an in-run ModeMismatch (T0012), got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (2) byte producer into byte consumer typechecks via harvested scheme ────

/// `let f = { echo hi }` then `$f | wc -l`: the harvested scheme for `f`
/// is `F[∅,Bytes]`, so the byte channel connects to `wc`'s byte input.
/// The checker sees the scheme, not a fresh mode variable.
#[test]
fn byte_producer_into_byte_consumer_typechecks() {
    let mut sh = shell();
    run(&mut sh, "let f = { echo hi }").unwrap();
    assert!(
        check_errors(&sh, "$f | wc -l").is_empty(),
        "expected a clean byte→byte pipeline across runs, got: {:?}",
        check_errors(&sh, "$f | wc -l")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (3) cross-run value-type error ─────────────────────────────────────────

/// A run-*N* binding used at a clashing value type in run *N+1* is
/// reported statically: `x` is `String` (the literal `'hello'`), so
/// `$x + 1` is a String-vs-Int mismatch.  The suite's established clash
/// is `String + Int` (see `typecheck.rs`), not the `Int + "hi"` the ADR
/// sketches — the language has no string literal inside `$[…]`.
#[test]
fn cross_run_value_type_error_is_static() {
    let mut sh = shell();
    run(&mut sh, "let xv = 'hello'").unwrap();
    let errs = check_errors(&sh, "return $[$xv + 1]");
    assert!(
        errs.iter()
            .any(|e| e.kind.render_message().contains("couldn't match")),
        "expected a cross-run value-type mismatch, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (4) rebind retypes the name ─────────────────────────────────────────────

/// `let x = 3` then `let x = 'hello'`: the rebind replaces the scheme,
/// so the next run checks `$x` against `String`.  `$[$x + 1]` is then a
/// mismatch, while passing `$x` to the String-consuming builtin `upper`
/// is clean.  The bare `x = …` assignment form the ADR sketches parses
/// as a command application, not a rebind, so the rebind is spelled
/// `let`; the language has no `++` concatenation operator, so the clean
/// String use is `upper`.
#[test]
fn rebind_retypes_the_name() {
    let mut sh = shell();
    run(&mut sh, "let xv = 3").unwrap();
    run(&mut sh, "let xv = 'hello'").unwrap();
    let errs = check_errors(&sh, "return $[$xv + 1]");
    assert!(
        errs.iter()
            .any(|e| e.kind.render_message().contains("couldn't match")),
        "expected $xv (now String) + 1 to mismatch, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    assert!(
        check_errors(&sh, "!{upper $xv}").is_empty(),
        "expected String use of $xv to be clean, got: {:?}",
        check_errors(&sh, "!{upper $xv}")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (5) a failed statement installs nothing ─────────────────────────────────

/// `let x = 1` then a run `nonexistent-command-zz; let x = hello` that
/// fails before the rebind: `$x` keeps the `Int` scheme.  The failed
/// statement installed neither value nor scheme.  The rebind is spelled
/// `let` (bare `x = …` is a command application, not a rebind); the eval
/// run is expected to error, and the binding must survive unchanged.
#[test]
fn failed_statement_installs_no_scheme() {
    let mut sh = shell();
    run(&mut sh, "let xv = 1").unwrap();
    // The bad command aborts the run before the rebind runs.
    let outcome = run(&mut sh, "nonexistent-command-zz\nlet xv = hello");
    assert!(outcome.is_err(), "the bad command must abort the run");
    // `xv` is still Int, so Int arithmetic is clean.
    assert!(
        check_errors(&sh, "return $[$xv + 1]").is_empty(),
        "expected $xv to keep its Int scheme after the failed rebind, got: {:?}",
        check_errors(&sh, "return $[$xv + 1]")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (6) pattern binds carry no scheme ───────────────────────────────────────

/// `let [a, b] = [1, 2]` then `$a + 1`: cross-run use of a pattern-bound
/// name neither errors spuriously nor sees a scheme — the destructuring
/// components are monomorphic and cannot be closed, so they carry no
/// scheme and elaborate as a bare variable at a fresh type.
#[test]
fn pattern_binds_carry_no_scheme() {
    let mut sh = shell();
    run(&mut sh, "let [a, bb] = [1, 2]").unwrap();
    assert!(
        check_errors(&sh, "return $[$a + 1]").is_empty(),
        "expected pattern-bound $a to check cleanly, got: {:?}",
        check_errors(&sh, "return $[$a + 1]")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    assert!(
        scheme_of(&sh, "a").is_none(),
        "a pattern-bound name must carry no scheme"
    );
    // A plain `let` binding does carry a scheme.
    run(&mut sh, "let n = 3").unwrap();
    assert!(
        scheme_of(&sh, "n").is_some(),
        "a plain let binding must carry a scheme"
    );
}

// ─── (7) alias visibility ────────────────────────────────────────────────────

/// `alias three { |args| echo 3; return 3 }` is visible to the next run's
/// check: the arm preserves `three`'s external byte-output mode while its
/// scheme records the `Int` value type, so `$[!{three} + 0]` typechecks
/// (the scheme says `Int`), while `three` alone is clean too.  After
/// `unalias three` the name falls back to external typing — a `String`
/// result — so the same arithmetic probe now clashes.
#[test]
fn alias_visible_to_next_run() {
    let mut sh = shell();
    run(&mut sh, "alias three { |args| echo 3; return 3 }").unwrap();
    assert!(
        check_errors(&sh, "three").is_empty(),
        "expected the alias used alone to be clean, got: {:?}",
        check_errors(&sh, "three")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    assert!(
        check_errors(&sh, "return $[!{three} + 0]").is_empty(),
        "expected the alias's recorded Int value type to admit `+ 0`, got: {:?}",
        check_errors(&sh, "return $[!{three} + 0]")
            .iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    run(&mut sh, "unalias three").unwrap();
    let errs = check_errors(&sh, "return $[!{three} + 0]");
    assert!(
        !errs.is_empty(),
        "expected `three` to fall back to external `String` typing after unalias, \
         making `+ 0` a clash; got no error"
    );
}

/// A value-output alias body defines the unknown head's modes, so the
/// definition draws no mismatch at the next run's check; the `∅` output is
/// rejected only where it feeds a byte consumer (`docs/SPEC.md` §4.2.1), as
/// `three | from-json` shows.
#[test]
fn value_output_alias_is_use_site_mode_mismatch() {
    let sh = shell();
    let errs = check_errors(&sh, "alias three { |args| return 3 }\nreturn unit");
    assert!(
        !is_mode_mismatch(&errs),
        "expected no ModeMismatch defining a value-output alias, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
    let errs = check_errors(&sh, "alias three { |args| return 3 }\nthree | from-json");
    assert!(
        is_mode_mismatch(&errs),
        "expected a ModeMismatch (T0012) where the value-output alias feeds from-json, got: {:?}",
        errs.iter()
            .map(|e| e.kind.render_message())
            .collect::<Vec<_>>()
    );
}

// ─── (8) bake/run harvest unity ─────────────────────────────────────────────

/// The scheme installed on a live scope binding renders identically to
/// the baked entry harvested at build time — both come from the same
/// `Bind`-node harvest.
#[test]
fn live_binding_scheme_matches_baked_entry() {
    let sh = shell();
    let baked: std::collections::HashMap<&str, String> = common::prelude_schemes()
        .iter()
        .map(|(n, s)| (n.as_str(), fmt_scheme(s)))
        .collect();
    for name in ["lines", "reverse"] {
        let live = scheme_of(&sh, name).unwrap_or_else(|| {
            panic!("prelude binding {name:?} must be bound on the live scope and carry a scheme")
        });
        assert_eq!(
            fmt_scheme(&live),
            baked[name],
            "live scope scheme for {name:?} must match the baked entry"
        );
    }
}
