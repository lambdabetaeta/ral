//! The argv convention at run time: what a name taking an argv actually
//! receives, and where an argv stops being text the shell may render freely.
//!
//! A handler arm, a base frame and an external are variadic over an argv, and
//! one renderer serves all three inside the shell — `Value::render_argv`, total
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
//! `tests/lang/argv.ral`; the tests below keep what a golden cannot say, the
//! hint on the refusal and the absence of a static diagnostic.
//!
//! Everything here drives the public `run` door, so each test is the session a
//! user has.

mod common;

use common::fresh_shell;

use ral_core::protocol::{Program, Run};
use ral_core::types::{Break, GrantStack, Value};
use ral_core::{
    CompileError, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Settled,
    StaticDiagnostics,
};

/// A session as every front end builds one: prelude registered, env seeded,
/// capabilities at root.
/// One top-level dispatch through the public door, stdout captured — the report
/// as a front end receives it, diagnosed or run.
fn report(src: &str) -> RunReport {
    fresh_shell().run(RunRequest {
        run: Run {
            program: Program::Source(src.into()),
            script_name: "<argv-convention>".into(),
            caps: GrantStack::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
    })
}

/// A run that must reach the evaluator, with what it wrote to stdout.
fn run_capture(src: &str) -> (Settled<Value>, String) {
    match report(src) {
        RunReport::Ran {
            ending, captured, ..
        } => {
            let stdout = captured.map(|c| c.stdout).unwrap_or_default();
            (
                ending.into_result(),
                String::from_utf8(stdout).expect("captured stdout is UTF-8"),
            )
        }
        RunReport::Static { diagnostics, .. } => panic!(
            "{src:?} must reach the evaluator, got {}",
            ral_core::diagnostic::format_static_diagnostics(&diagnostics).0
        ),
    }
}

/// The type diagnostics `src` earns without ever reaching the evaluator, by
/// code — empty when it does reach it.
fn static_codes(src: &str) -> Vec<String> {
    match report(src) {
        RunReport::Static {
            diagnostics:
                StaticDiagnostics::Compile {
                    error: CompileError::Types(errors),
                    ..
                },
            ..
        } => errors.iter().map(|e| e.kind.code().to_string()).collect(),
        RunReport::Static {
            diagnostics:
                StaticDiagnostics::Compile {
                    error: CompileError::Parse(error),
                    ..
                },
            ..
        } => panic!("{src:?}: expected type diagnostics, got parse {error:?}"),
        RunReport::Static {
            diagnostics: StaticDiagnostics::Host(e),
            ..
        } => panic!("{src:?}: expected type diagnostics, got host {e:?}"),
        RunReport::Ran { .. } => Vec::new(),
    }
}

/// A list is refused there too, and the refusal names the notation that lowers
/// it — so the gate teaches the argv the user meant.
#[test]
fn the_exec_boundary_names_the_spread_that_lowers_a_list() {
    match run_capture("let show = { |v| cat $v }; show [a, b]").0 {
        Err(Break::Error(e)) => assert!(
            e.hint.is_some_and(|h| h.contains("...")),
            "the refusal should point at `...`"
        ),
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
    assert!(
        static_codes("let show = { |v| cat $v }; show [a: 1]").is_empty(),
        "a parameter's shape is not known here, so nothing may be refused"
    );
    // Nor is a spread's elements' shape: an empty spread reaches `execve(2)`
    // cleanly, and a static refusal would reject a call that runs.
    assert!(
        static_codes("let none = filter { |xs| return $[false] } [[1], [2]]; cat ...$none")
            .is_empty(),
        "a spread is not gated"
    );
}
