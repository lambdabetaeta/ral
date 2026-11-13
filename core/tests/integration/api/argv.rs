//! The argv convention at run time: what a name taking an argv actually
//! receives, and where an argv stops being text the shell may render freely.
//!
//! A handler arm, a base frame and an external are variadic over an argv, and
//! one renderer serves all three inside the shell — `ValClos::render_argv`, total
//! by construction, so a map, a lambda and a block all have a text form.  The
//! exec boundary is the one place that is *not* total: heading for `execve(2)`,
//! it refuses the shapes an operating system has no argument for.  Total
//! rendering inside, gated at the OS call; a rule uniform across both would be
//! wrong in one direction.
//!
//! That gate is read twice.  A shape is what a type states, so wherever an
//! argument's type is concrete the checker refuses it before the run (T0057);
//! where polymorphism hides it, the pre-spawn refusal is still there to catch it.
//! One refused set, two moments.  The run-time half — what an arm receives,
//! total rendering, the refusal at the exec boundary — is the golden
//! `tests/run/lang/argv.ral`; the tests below keep what a golden cannot say, the
//! hint on the refusal and the absence of a static diagnostic.
//!
//! Everything here drives the public `run` entry, so each test is the session a
//! user has.

#[cfg(unix)]
use crate::common::under_grant;

use ral_core::Settled;
use ral_core::ValClos;
use ral_core::protocol::Run;
use ral_core::shell::run::{RunReport, StaticDiagnostics};
use ral_core::shell::runtime::Break;
use ral_core::source::SourceOrigin;
use ral_core::test_helper::fresh_shell;

/// One top-level dispatch through the public entry, stdout captured — the report
/// as a front end receives it, diagnosed or run.
fn report(src: &str) -> RunReport {
    fresh_shell().run(Run::captured(src, SourceOrigin::named("<argv-convention>")))
}

/// A run that must reach the evaluator, with what it wrote to stdout.
fn run_capture(src: &str) -> (Settled<ValClos>, String) {
    match report(src) {
        RunReport::Ran {
            ending, captured, ..
        } => {
            let stdout = captured.map(|c| c.stdout.bytes).unwrap_or_default();
            (
                ending.into_settled(),
                String::from_utf8(stdout).expect("captured stdout is UTF-8"),
            )
        }
        RunReport::Checked { .. } => panic!("source fixture must run, not only check: {src:?}"),
        RunReport::Static { diagnostics, .. } => panic!(
            "{src:?} must reach the evaluator, got {}",
            diagnostics
                .render(&ral_core::shell::runtime::ExitHints::default())
                .0
        ),
    }
}

/// The type diagnostics `src` earns without ever reaching the evaluator, by
/// code — empty when it does reach it.
fn static_codes(src: &str) -> Vec<String> {
    match report(src) {
        RunReport::Static {
            diagnostics: StaticDiagnostics::Compile(r),
            ..
        } if r.status.get() == 1 => r
            .reports
            .iter()
            .filter_map(|r| r.code.map(String::from))
            .collect(),
        RunReport::Static {
            diagnostics: StaticDiagnostics::Compile(r),
            ..
        } => panic!(
            "{src:?}: expected type diagnostics, got parse {:?}",
            r.reports
        ),
        RunReport::Static {
            diagnostics: StaticDiagnostics::Host(e),
            ..
        } => panic!("{src:?}: expected type diagnostics, got host {e:?}"),

        RunReport::Ran { .. } => Vec::new(),
        RunReport::Checked { .. } => panic!("source fixture must run, not only check: {src:?}"),
    }
}

/// A list is refused there too, and the refusal names the notation that lowers
/// it — so the gate teaches the argv the user meant.
#[test]
fn the_exec_boundary_names_the_spread_that_lowers_a_list() {
    match run_capture("let show = { |v| cat $v }; show [a, b]").0 {
        Err(Break::Error(e)) => {
            assert!(e.hints().is_empty(), "intrinsic advice is not cached");
            let advice = e.reason.advice();
            assert!(
                advice.first().is_some_and(|h| h.contains("...")),
                "the refusal should point at `...`"
            );
            assert!(e.plain().contains(&advice[0]));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ── What the checker leaves to the spawn ──────────────────────────────────────

/// And what the type does not say, the checker does not say either: a
/// parameter's shape is the run's business, so the same call passes the gate
/// and meets `vet` instead.  This is why no program that ran before stops
/// running.
#[test]
fn a_polymorphic_argument_is_left_to_the_spawn() {
    let call = "let show = { |v| cat $v }; show [a: 1]";
    assert!(
        static_codes(call).is_empty(),
        "a parameter's shape is not known here, so nothing may be refused"
    );
    match run_capture(call).0 {
        Err(Break::Error(e)) => {
            assert!(e.hints().is_empty(), "intrinsic advice is not cached");
            let advice = e.reason.advice();
            assert!(!advice.is_empty(), "the refusal carries the idiom");
            assert!(e.plain().contains(&advice[0]));
        }
        other => panic!("`vet` must refuse the shape at run time, got {other:?}"),
    }
    // Nor is a spread's elements' shape: an empty spread reaches `execve(2)`
    // cleanly, and a static refusal would reject a call that runs.
    assert!(
        static_codes("let none = filter { |xs| return $[false] } [[1], [2]]; cat ...$none")
            .is_empty(),
        "a spread is not gated"
    );
}

/// A grant refusal outranks an argument's runtime shape, alone and as a
/// direct stage.
#[cfg(unix)]
#[test]
fn a_grant_refusal_outranks_the_shape_of_an_argument() {
    for call in ["/bin/echo $v", "/bin/echo $v | /bin/cat"] {
        let source = format!(
            "let show = {{ |v| {call} }}; {}",
            under_grant("show [a: 1]")
        );
        match run_capture(&source).0 {
            Err(Break::Error(e)) => assert!(
                e.describe().contains("denied by active grant"),
                "{call}: {}",
                e.describe()
            ),
            other => panic!("{call}: expected a grant refusal, got {other:?}"),
        }
    }
}

#[test]
fn every_refused_argv_shape_derives_its_unchanged_remedy_after_transport() {
    use ral_core::data::datum::Datum;
    use ral_core::fact::{
        DataStep, Expected, Found, InvalidCause, InvalidFailure, Reason, ShapeFailure, ValueShape,
    };
    for (found, remedy) in [
        (
            Found::Unit,
            "`()` is nothing, not a word: if the text is meant, write `'()'`",
        ),
        (
            Found::List,
            "use '...' to spread a list into arguments: tool ...$xs",
        ),
        (
            Found::Map,
            "a map is fields rather than one word: pass a field, as in `tool $m[name]`, or render the whole of it with `tool !{to-json $m}`",
        ),
        (
            Found::Block,
            "a block is a computation, not a word: run it and pass what it gives, as in `tool !{!$b}`",
        ),
        (
            Found::Function,
            "a block is a computation, not a word: run it and pass what it gives, as in `tool !{!$b}`",
        ),
        (
            Found::Handle,
            "await the concurrent block first (`let r = await $h`), then pass what it returned, as in `tool $r`",
        ),
        (
            Found::Bytes,
            "pipe binary data via stdin with to-bytes, or decode to string first",
        ),
    ] {
        let description = format!(
            "cannot pass {} as argument at index 0 to `tool`",
            found.describe()
        );
        let reason = Reason::Invalid(Box::new(InvalidFailure {
            operation: "tool".into(),
            path: vec![DataStep::Index(0usize.into())],
            cause: InvalidCause::Shape(ShapeFailure {
                expected: Expected::AnyOf(vec![
                    ValueShape::Bool,
                    ValueShape::Int,
                    ValueShape::Float,
                    ValueShape::String,
                    ValueShape::Variant,
                ]),
                found,
            }),
        }));
        let decoded = Reason::decode(&reason.clone().encode()).unwrap();
        assert_eq!(decoded, reason);
        assert_eq!(decoded.describe(), description);
        assert_eq!(decoded.advice(), [remedy]);
        let error = ral_core::Error::new(decoded);
        assert_eq!(error.hints(), []);
        let rendered = error.render(
            &ral_core::source::SourceDb::default(),
            None,
            &ral_core::shell::runtime::ExitHints::default(),
        );
        assert!(rendered.contains(&description), "{rendered}");
        assert_eq!(rendered.matches(remedy).count(), 1, "{rendered}");
    }
}
