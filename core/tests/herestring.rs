//! Behavioral contract of the here-string redirect `<< str`: the string
//! value — literal or stored — becomes the command's stdin, with one
//! newline at the very front of the value dropped so a multiline body can
//! start on the line below the command.  Raw strings themselves stay
//! verbatim everywhere; the drop is a semantic property of `<<` alone,
//! applied to whatever string reaches it at evaluation.
//!
//! Tests drive the public `run_turn` door like a REPL turn or an exarch
//! tool call would; the in-process consumer is `from-string` (a codec that
//! drains stdin to a string), the external consumer is `cat`.

mod common;

use ral_core::io::TerminalState;
use ral_core::transport::{Program, Turn};
use ral_core::types::{Capabilities, Settled, Shell};
use ral_core::{RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin, Value};

fn fresh_shell() -> Shell {
    ral_core::driver::boot_shell(
        TerminalState::default(),
        common::prelude(),
        &ral_core::driver::HostSurface::default(),
    )
}

fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run_turn(TurnRequest {
        turn: Turn {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            turn_limit: None,
            deferred_lease: None,
            worker_cap: None,
            io: TurnIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: TurnStdin::Inherit,
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        TurnReport::Ran { result, .. } => result,
        TurnReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

fn expect_string(shell: &mut Shell, source: &str) -> String {
    match top_level(shell, source) {
        Ok(Value::String(s)) => s,
        other => panic!("expected a string from {source:?}, got {other:?}"),
    }
}

/// A single-line literal body arrives verbatim.
#[test]
fn literal_body_feeds_stdin() {
    let mut shell = fresh_shell();
    assert_eq!(
        expect_string(&mut shell, "from-string << #'hello'#"),
        "hello"
    );
}

/// One newline at the front of the payload is dropped — the bash-heredoc
/// transcription shape, body starting on the line below the command —
/// and only one: the rest of the body is untouched, trailing newline
/// included.
#[test]
fn opener_newline_is_dropped() {
    let mut shell = fresh_shell();
    assert_eq!(
        expect_string(&mut shell, "from-string << #'\nline1\nline2\n'#"),
        "line1\nline2\n"
    );
    assert_eq!(
        expect_string(&mut shell, "from-string << #'\n\nblank first line\n'#"),
        "\nblank first line\n"
    );
}

/// The drop is a property of `<<`, not of the literal: a stored string
/// with a leading newline loses it the same way.
#[test]
fn stored_string_payload() {
    let mut shell = fresh_shell();
    assert_eq!(
        expect_string(&mut shell, "let body = \"\\nabc\"\nfrom-string << $body"),
        "abc"
    );
    assert_eq!(
        expect_string(&mut shell, "let plain = \"abc\"\nfrom-string << $plain"),
        "abc"
    );
}

/// A CRLF opener newline is dropped whole, never split.
#[test]
fn crlf_opener_newline_is_dropped() {
    let mut shell = fresh_shell();
    assert_eq!(
        expect_string(&mut shell, "let body = \"\\r\\nabc\"\nfrom-string << $body"),
        "abc"
    );
}

/// A payload far past the kernel pipe buffer must not deadlock the shell:
/// the writer side lives on its own thread.
#[test]
fn large_payload_does_not_deadlock() {
    let mut shell = fresh_shell();
    let body = "x".repeat(256 * 1024);
    let got = expect_string(&mut shell, &format!("from-string << #'{body}'#"));
    assert_eq!(got, body);
}

/// An external child reads the here-string as its stdin.
#[cfg(unix)]
#[test]
fn external_child_reads_herestring() {
    let mut shell = fresh_shell();
    assert_eq!(
        expect_string(&mut shell, "cat << #'\nfrom the body\n'# | from-string"),
        "from the body\n"
    );
}

/// Several fd-0 redirects compose POSIX-style: the last one wins,
/// whichever of `< file` / `<< str` it is.
#[cfg(unix)]
#[test]
fn last_stdin_redirect_wins() {
    let mut shell = fresh_shell();
    assert_eq!(
        expect_string(&mut shell, "from-string < /dev/null << #'won'#"),
        "won"
    );
    assert_eq!(
        expect_string(&mut shell, "from-string << #'lost'# < /dev/null"),
        ""
    );
}
