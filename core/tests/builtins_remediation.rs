#![allow(clippy::disallowed_methods)]

//! The builtins-remediation checks that need the harness: an `exit` status
//! is an escape rather than an error, and two shapes are refused by the
//! checker before any run.  The runtime refusals (`dedent`, `range`, `int`,
//! `float`, `from-json`, …) are goldens in `tests/builtins`
//! (`text-edges`, `numeric`, `codec-errors`, `comparison`).
//!
//! Each source string goes through the public `run` door like a REPL run.

mod common;

use common::fresh_shell;

use ral_core::protocol::{Program, Run};
use ral_core::types::{Break, Escape, GrantStack, Settled, Shell, Value};
use ral_core::{
    CompileError, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin,
    StaticDiagnostics,
};

/// Run one top-level run of `source` through the public `run` door
/// and return the body's `Settled<Value>`.  Every test below picks source
/// it expects to compile, so a static diagnostic is a test bug.
fn eval(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
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
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// Run `source` expecting it to be rejected before evaluation, and hand back
/// the diagnostics that refused it.
fn expect_static(source: &str) -> StaticDiagnostics {
    let mut shell = fresh_shell();
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
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
        RunReport::Static { diagnostics } => diagnostics,
        RunReport::Ran { ending, .. } => {
            panic!("{source:?}: expected a static rejection, got {ending:?}")
        }
    }
}

/// Run `source` expecting one type error, whose code must be `code`.
fn expect_type_code(source: &str, code: &str) {
    let codes: Vec<_> = match expect_static(source) {
        StaticDiagnostics::Compile {
            error: CompileError::Types(errors),
            ..
        } => errors.iter().map(|e| e.kind.code()).collect(),
        StaticDiagnostics::Compile {
            error: CompileError::Parse(error),
            ..
        } => {
            panic!("{source:?}: expected a type error, got parse {error:?}")
        }
        StaticDiagnostics::Host(e) => panic!("{source:?}: expected a type error, got host {e:?}"),
    };
    assert_eq!(codes, [code], "{source:?}");
}

#[test]
fn exit_preserves_in_range_status() {
    let mut shell = fresh_shell();
    match eval(&mut shell, "exit 7") {
        Err(Break::Escape(Escape::Exit(code))) => assert_eq!(code, 7),
        other => panic!("exit 7: expected Escape::Exit(7), got {other:?}"),
    }
}

/// A bare status never reaches `fail`'s body: the error-record shape is in the
/// signature, so the run stops at the type error.
#[test]
fn fail_bare_forms_are_rejected() {
    expect_static("fail 7");
    expect_static("fail \"boom\"");
}

/// An encoder takes its one argument by application: it has no argv, so `...`
/// has nothing to spread into and the checker refuses the call outright.
#[test]
fn a_spread_never_reaches_an_encoder() {
    expect_type_code("let nothing = []\nto-json ...$nothing\nreturn ()", "T0056");
}
