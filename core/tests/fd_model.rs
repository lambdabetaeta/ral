//! ral's fd model, driven through the public run door.
//!
//! A redirect is one of stdin, stdout, stderr or `2>&1`; the identity dups
//! `1>&1` and `2>&2` denote nothing.  The parser eliminates fd numbers into
//! that sum and refuses any that names no stream, as `1< f` does; these tests
//! read the rule from outside.

mod common;

use common::fresh_shell;

use ral_core::protocol::{Program, Run};
use ral_core::types::{GrantStack, Shell};
use ral_core::{
    CompileError, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin,
    StaticDiagnostics,
};

fn report(shell: &mut Shell, source: &str) -> RunReport {
    shell.run(RunRequest {
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
    })
}

/// The parse error's message, or a panic: `RunReport` carries no `Debug`, so
/// the failure says what was expected rather than what arrived.
fn parse_error(shell: &mut Shell, source: &str) -> String {
    match report(shell, source) {
        RunReport::Static {
            diagnostics:
                StaticDiagnostics::Compile {
                    error: CompileError::Parse(error),
                    ..
                },
        } => error.message,
        _ => panic!("expected a parse error from {source:?}"),
    }
}

/// With an ear listening, the write observation for a fd-prefixed read
/// would reach `mode_str` were it not refused at the parser first: the run
/// never starts, and the session is still usable afterwards.
#[test]
fn fd_prefixed_read_is_a_parse_error_and_the_session_survives() {
    let mut shell = fresh_shell();
    for source in [
        "audit { /bin/cat 1< f }",
        "audit { /bin/cat 2< f }",
        "/bin/cat 1< f",
    ] {
        let message = parse_error(&mut shell, source);
        assert!(
            message.contains("always feeds standard input"),
            "{source:?} gave {message:?}"
        );
    }
    // Same shell: the refusals cost it nothing.
    assert!(matches!(
        report(&mut shell, "/bin/echo hi"),
        RunReport::Ran { .. }
    ));
}

/// Standard input is read-only, so an fd-0 write door has nothing to mean
/// either — the same rule, the other way round.
#[test]
fn writing_to_fd_zero_is_a_parse_error() {
    let mut shell = fresh_shell();
    for source in [
        "/bin/echo hi 0> f",
        "/bin/echo hi 0>> f",
        "/bin/echo hi 0>~ f",
    ] {
        let message = parse_error(&mut shell, source);
        assert!(
            message.contains("standard input cannot be written to"),
            "{source:?} gave {message:?}"
        );
    }
}

/// An fd prefix on `<<` is the same mistake with its own advice, and it wins
/// over the heredoc diagnostic the payload would otherwise earn.
#[test]
fn fd_prefixed_herestring_names_the_prefix() {
    let mut shell = fresh_shell();
    let message = parse_error(&mut shell, "/bin/cat 1<< body");
    assert!(
        message.contains("drop the file-descriptor prefix"),
        "got {message:?}"
    );
}

/// The identity dups stay legal: what bash means by them — nothing — is what
/// ral means by them, so there is nothing to refuse.  `2>&1` is the one dup
/// that does something.
#[test]
fn identity_dups_and_the_stderr_fold_are_admitted() {
    for source in [
        "/bin/echo hi >&1",
        "/bin/echo hi 1>&1",
        "/bin/echo hi 2>&2",
        "/bin/echo hi 2>&1",
    ] {
        assert!(
            ral_core::syntax::parser::parse(source).is_ok(),
            "{source:?} should parse"
        );
    }
}
