#![allow(clippy::disallowed_methods, reason = "test code")]

//! Builtin call forms that need the harness: an `exit` status is an escape
//! rather than an error, and a spread into an encoder is refused by the
//! checker before any run.  The runtime refusals (`dedent`, `range`, `int`,
//! `float`, `from-json`, …) are goldens in `tests/builtins`
//! (`text-edges`, `numeric`, `codec-errors`, `comparison`).
//!
//! Each source string goes through the public `run` entry like a REPL run.

mod common;

use common::{fresh_shell, top_level};

use ral_core::protocol::Run;
use ral_core::shell::run::{RunReport, StaticDiagnostics};
use ral_core::shell::runtime::{Break, Escape};
use ral_core::source::SourceOrigin;

/// Run `source` expecting it to be rejected before evaluation, and hand back
/// the diagnostics that refused it.
fn expect_static(source: &str) -> StaticDiagnostics {
    let mut shell = fresh_shell();
    match shell.run(Run::foreground(source, SourceOrigin::named("<test>"))) {
        RunReport::Static { diagnostics } => diagnostics,
        RunReport::Ran { ending, .. } => {
            panic!("{source:?}: expected a static rejection, got {ending:?}")
        }
    }
}

/// Run `source` expecting one type error, whose code must be `code`.
fn expect_type_code(source: &str, code: &str) {
    let codes: Vec<_> = match expect_static(source) {
        StaticDiagnostics::Compile(r) if r.status.get() == 1 => {
            r.reports.iter().filter_map(|r| r.code).collect()
        }
        StaticDiagnostics::Compile(r) => {
            panic!(
                "{source:?}: expected a type error, got parse {:?}",
                r.reports
            )
        }
        StaticDiagnostics::Host(e) => panic!("{source:?}: expected a type error, got host {e:?}"),
    };
    assert_eq!(codes, [code], "{source:?}");
}

#[test]
fn exit_preserves_in_range_status() {
    let mut shell = fresh_shell();
    match top_level(&mut shell, "exit 7") {
        Err(Break::Escape(Escape::Exit(code))) => assert_eq!(code, 7),
        other => panic!("exit 7: expected Escape::Exit(7), got {other:?}"),
    }
}

/// An encoder takes its one argument by application: it has no argv, so `...`
/// has nothing to spread into and the checker refuses the call outright.
#[test]
fn a_spread_never_reaches_an_encoder() {
    expect_type_code("let nothing = []\nto-json ...$nothing\nreturn ()", "T0056");
}
