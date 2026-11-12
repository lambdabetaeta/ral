//! Escapes observable at the public `run` entry: `guard` under an `exit`, an
//! `exit` status as an escape rather than an error, and handler self-masking
//! surviving a panic mid-body.

use crate::common::top_level;
use ral_core::ValClos;
use ral_core::fact::FailureCode as _;
use ral_core::io::terminal::TerminalState;
use ral_core::shell::boot::{HostSurface, boot_shell};
use ral_core::shell::runtime::{Break, Escape, Mooring, Shell};
use ral_core::test_helper::fresh_shell;

const HOST: &str = "a test host resolves its baseline";

// ── handler self-masking survives a panic mid-body ──
//
// `machine::apply_handler` lifts the matched handler frame off the stack for the
// dynamic extent of the body (so a same-name call from inside reaches
// the next outer match), then re-inserts it via an RAII guard whose
// `Drop` runs on any exit, panic or otherwise.  A stripped persistent frame
// is a permanently deleted user handler, the one piece of dynamic context
// with no save elsewhere to rebuild from, and exarch `catch_unwind`s
// evaluation and continues the session on the same `Shell`, so a caught
// panic must not silently delete the user's handler.

/// A nullary host builtin whose reducer raises a Rust panic.  Registered
/// only by the test below, it is the panic trigger the RAII guard needs:
/// no shipped builtin should panic, so the test owns one rather than
/// leaning on whichever builtin happens to be panic-prone.
fn panic_builtin(
    _args: &[ValClos],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> ral_core::shell::runtime::Settled<ValClos> {
    panic!("__test-panic builtin invoked");
}

static PANIC_BUILTIN_ARR: [ral_core::builtin::BuiltinEntry; 1] =
    [ral_core::builtin::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("__test-panic"),
        ral_core::typecheck::builtins::scheme::diverges,
        "__test-panic  — test-only: raise a Rust panic.",
        ral_core::builtin::BuiltinBody::Static(panic_builtin),
    )];
static PANIC_BUILTIN: &[ral_core::builtin::BuiltinEntry] = &PANIC_BUILTIN_ARR;

/// A Rust panic raised inside a handler body must leave the handler
/// still installed.  The body invokes `__test-panic`, a host builtin the
/// test registers whose reducer panics unconditionally — any panic
/// mid-handler-body would do, and a dedicated trigger keeps the test
/// independent of which shipped builtin happens to be panic-prone.  The
/// `catch_unwind` stands in for exarch's `pump`, which catches and
/// continues on the same `Shell`.
#[test]
fn handler_self_mask_survives_panic_mid_body() {
    let mut shell = boot_shell(
        TerminalState::default(),
        crate::common::prelude(),
        &HostSurface {
            statics: vec![PANIC_BUILTIN],
            ..HostSurface::default()
        },
        ral_core::test_helper::process_cwd(),
    )
    .expect(HOST);
    let arm = top_level(&mut shell, "return { |args| __test-panic }")
        .expect("creating the handler arm must succeed");
    shell
        .install_handler("boom".into(), arm)
        .expect("installing the handler must succeed");
    assert!(
        shell.has_handler("boom"),
        "handler must be installed before the call"
    );

    // The dispatch of `boom` runs `machine::apply_handler`, whose `Unmask`
    // frame strips the handler, then applies the body — which panics.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        top_level(&mut shell, "boom")
    }));

    assert!(
        outcome.is_err(),
        "the handler body's `__test-panic` must raise; its reducer panics \
         unconditionally"
    );
    assert!(
        shell.has_handler("boom"),
        "the handler `boom` must still be installed after a panic unwound \
         through its body: the RAII guard restores the stripped frame from \
         `Drop`, where straight-line re-insertion would be skipped"
    );
}

/// A cleanup runs under an `exit`, and the `exit` follows it.
#[test]
fn a_guard_cleanup_runs_under_an_exit_and_the_exit_follows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mark = dir.path().join("cleaned");
    let src = format!(
        "guard {{ () }} {{ |_| exit 3 }} {{ |_ _| echo cleaned > '{}' }}",
        mark.display()
    );
    match top_level(&mut fresh_shell(), &src) {
        Err(Break::Escape(Escape::Exit(3))) => {}
        other => panic!("expected exit 3, got {other:?}"),
    }
    assert!(mark.exists(), "the cleanup ran");
}

/// A cleanup's own failure pre-empts the halt it ran under.
#[test]
fn a_cleanups_fail_pre_empts_an_exit() {
    match top_level(
        &mut fresh_shell(),
        "guard { () } { |_| exit 3 } { |_ _| fail x }",
    ) {
        Err(Break::Error(e)) => assert_eq!(e.failure_code().get(), 1),
        other => panic!("expected an error with code 1, got {other:?}"),
    }
}

#[test]
fn exit_preserves_in_range_status() {
    let mut shell = fresh_shell();
    match top_level(&mut shell, "exit 7") {
        Err(Break::Escape(Escape::Exit(code))) => assert_eq!(code, 7),
        other => panic!("exit 7: expected Escape::Exit(7), got {other:?}"),
    }
}
