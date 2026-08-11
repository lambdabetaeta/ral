#![allow(clippy::disallowed_methods)]

//! The byte-to-value coercion is a translation, not a call: what the checker
//! composes onto a byte-routed `let` means the same thing in every session.
//!
//! `let x = echo hi` compiles to `Decode(Capture(echo hi))`, two nodes of
//! syntax. Nothing here is a name, so there is nothing for an alias, a
//! binding, or a handler frame to stand in for; and nothing is bound, so a
//! session cannot see the bytes on their way to becoming a `String`.
//!
//! Drives the public `run` door, so each test is the session a user has.

mod common;

use ral_core::transport::{Program, Run};
use ral_core::types::{Capabilities, Shell};
use ral_core::{
    RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, StaticDiagnostics, Value,
};

/// A session as every front end builds one: prelude registered, env seeded,
/// capabilities at root.
fn fresh_shell() -> Shell {
    ral_core::boot::boot_shell(
        ral_core::io::TerminalState::default(),
        common::prelude(),
        &ral_core::boot::HostSurface::default(),
    )
}

/// One top-level run through the public door. A static diagnostic and a
/// runtime failure both come back as `Err(message)`, so a test can insist on
/// *an error the session survives* without caring which layer refused.
fn run(shell: &mut Shell, source: &str) -> Result<Value, String> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
            script_name: "<capture-coercion>".into(),
            caps: Capabilities::root(),
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
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { ending, .. } => ending.into_result().map_err(|e| format!("{e:?}")),
        RunReport::Static { diagnostics, .. } => Err(match diagnostics {
            StaticDiagnostics::Parse(e) => format!("parse: {e:?}"),
            StaticDiagnostics::Types(errs) => format!("types: {} diagnostic(s)", errs.len()),
            StaticDiagnostics::Host(e) => format!("host: {e:?}"),
        }),
    }
}

fn ok(shell: &mut Shell, source: &str) -> Value {
    run(shell, source).unwrap_or_else(|e| panic!("{source:?} must run: {e}"))
}

/// The base case the rest are measured against: the captured bytes reach the
/// binding as text, with the command's trailing newline dropped.
#[test]
fn a_byte_routed_let_binds_the_decoded_text() {
    let mut shell = fresh_shell();
    assert_eq!(
        ok(&mut shell, "let captured = echo hi\nreturn $captured"),
        Value::String("hi".into())
    );
}

/// An alias spelled like the machinery the coercion used to call cannot be
/// reached by it: the captured `let` still binds `"hi"`, the `String` the
/// checker promised, and not the alias's own value.
#[test]
fn an_alias_cannot_stand_in_for_the_coercion() {
    let mut shell = fresh_shell();
    ok(
        &mut shell,
        "alias __decode-captured { |_| return 42 }\nreturn unit",
    );
    assert_eq!(
        ok(&mut shell, "__decode-captured anything"),
        Value::Int(42),
        "the alias must really be installed and dispatchable, or this test proves nothing"
    );
    assert_eq!(
        ok(&mut shell, "let captured = echo hi\nreturn $captured"),
        Value::String("hi".into())
    );
}

/// The blanking case, which is worse than the wrong-type one: an alias that
/// returns the empty string once emptied every captured binding in scope.
#[test]
fn an_alias_cannot_blank_a_captured_binding() {
    let mut shell = fresh_shell();
    ok(
        &mut shell,
        "alias __decode-captured { |_| return \"\" }\nreturn unit",
    );
    assert_eq!(
        ok(&mut shell, "let greeting = echo hello\nreturn $greeting"),
        Value::String("hello".into())
    );
}

/// A handler frame is the other half of the resolution chain an `Exec` head
/// walks, so it gets the same test as the alias.
#[test]
fn a_handler_frame_cannot_stand_in_for_the_coercion() {
    let mut shell = fresh_shell();
    let frame = "within [handlers: [__decode-captured: { |_| return 42 }]]";
    assert_eq!(
        ok(
            &mut shell,
            &format!("{frame} {{ __decode-captured anything }}")
        ),
        Value::Int(42),
        "the frame must really intercept the name, or this test proves nothing"
    );
    assert_eq!(
        ok(
            &mut shell,
            &format!("{frame} {{ let captured = echo hi\n return $captured }}")
        ),
        Value::String("hi".into())
    );
}

/// The coercion binds nothing, so a user binding of the name it once
/// synthesized keeps its value across a byte-routed `let` — which once
/// overwrote it with the captured bytes.
#[test]
fn a_binding_named_like_the_old_synthesized_binder_survives() {
    let mut shell = fresh_shell();
    ok(&mut shell, "let __captured = 7\nreturn unit");
    assert_eq!(
        ok(&mut shell, "let captured = echo hi\nreturn $__captured"),
        Value::Int(7)
    );
    assert_eq!(
        ok(&mut shell, "return $captured"),
        Value::String("hi".into())
    );
}

/// And nothing is left behind: after a byte-routed `let`, no binding the
/// coercion made is in scope to be read.
#[test]
fn the_coercion_leaves_no_binding_in_scope() {
    let mut shell = fresh_shell();
    ok(&mut shell, "let captured = echo hi\nreturn unit");
    let leaked = run(&mut shell, "return $__captured");
    assert!(
        leaked.is_err(),
        "a captured let must leave no binding behind, but `$__captured` read {leaked:?}"
    );
}

/// The decoding step is not a command, so no program can call it — with a
/// spread argument list, which is how the old builtin was reached past its
/// static arity, least of all. The session survives the refusal, which is the
/// point: an error, not a torn-down run.
#[test]
fn the_coercion_is_not_a_command_a_program_can_call() {
    let mut shell = fresh_shell();
    let refused = run(&mut shell, "let e = []\n__decode-captured ...$e");
    assert!(
        refused.is_err(),
        "`__decode-captured` must not be a command, but the call returned {refused:?}"
    );
    assert_eq!(
        ok(&mut shell, "let captured = echo hi\nreturn $captured"),
        Value::String("hi".into()),
        "the session must still be alive after refusing the call"
    );
}
