//! Handler self-masking survives a panic mid-body, observable at the
//! public `run` entry.

#![allow(clippy::disallowed_methods, reason = "test code")]

mod common;

use ral_core::Value;
use ral_core::boot::{HostSurface, boot_shell};
use ral_core::io::terminal::TerminalState;
use ral_core::protocol::Run;
use ral_core::source::SourceOrigin;
use ral_core::types::{Break, Escape, Mooring, Settled, Shell};

const HOST: &str = "a test host resolves its floor";

/// Run one top-level run of `source` through the public `run` entry
/// and return the body's `Settled<Value>`.  Every test below picks source
/// it expects to compile, so a static diagnostic is a test bug.
fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    shell
        .run(Run::foreground(source, SourceOrigin::named("<test>")))
        .expect_ran()
        .into_result()
}

// ── handler self-masking survives a panic mid-body (finding E5; rec. A4) ──
//
// `machine::apply_handler` lifts the matched handler frame off the stack for the
// dynamic extent of the body (so a same-name call from inside reaches
// the next outer match), then re-inserts it via an RAII guard whose
// `Drop` runs on any exit, panic or otherwise.  A stripped *alias* frame
// is a permanently deleted user alias, the one piece of dynamic context
// with no save elsewhere to rebuild from, and exarch `catch_unwind`s
// evaluation and continues the session on the same `Shell`, so a caught
// panic must not silently delete the user's alias.

/// A nullary host builtin whose reducer raises a Rust panic.  Registered
/// only by the test below, it is the panic trigger the RAII guard needs:
/// no shipped builtin should panic, so the test owns one rather than
/// leaning on whichever builtin happens to be panic-prone.
fn panic_builtin(
    _args: &[Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> ral_core::types::Settled<Value> {
    panic!("__test-panic builtin invoked");
}

static PANIC_BUILTIN_ARR: [ral_core::types::BuiltinEntry; 1] =
    [ral_core::types::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("__test-panic"),
        ral_core::typecheck::builtins::scheme::diverges,
        "__test-panic  — test-only: raise a Rust panic.",
        ral_core::types::BuiltinBody::Static(panic_builtin),
    )];
static PANIC_BUILTIN: &[ral_core::types::BuiltinEntry] = &PANIC_BUILTIN_ARR;

/// E5 — a Rust panic raised inside an alias body must leave the alias
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
        common::prelude(),
        &HostSurface {
            statics: vec![PANIC_BUILTIN],
            ..HostSurface::default()
        },
        ral_core::test_helper::process_cwd(),
    )
    .expect(HOST);
    let _ = top_level(&mut shell, "alias boom { |args| __test-panic }")
        .expect("installing the alias must succeed");
    assert!(
        shell.has_alias("boom"),
        "alias must be installed before the call"
    );

    // The dispatch of `boom` runs `machine::apply_handler`, whose `Unmask`
    // frame strips the alias, then applies the body — which panics.  Default panic
    // output is noisy; silence the hook for the duration of the catch.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        top_level(&mut shell, "boom")
    }));
    std::panic::set_hook(prev_hook);

    assert!(
        outcome.is_err(),
        "the alias body's `__test-panic` must raise; its reducer panics \
         unconditionally"
    );
    assert!(
        shell.has_alias("boom"),
        "the alias `boom` must still be installed after a panic unwound \
         through its body.  Pre-A4 the stripped frame was re-inserted by \
         straight-line code skipped on the unwind, permanently deleting \
         the user alias; A4's RAII guard restores it from `Drop`."
    );
}

fn test_shell() -> Shell {
    boot_shell(
        TerminalState::default(),
        common::prelude(),
        &HostSurface::default(),
        ral_core::test_helper::process_cwd(),
    )
    .expect(HOST)
}

/// A cleanup runs under an `exit`, and the `exit` follows it.
#[test]
fn a_guard_cleanup_runs_under_an_exit_and_the_exit_follows() {
    let mut shell = test_shell();
    match top_level(&mut shell, "guard { exit 3 } { echo cleaned }") {
        Err(Break::Escape(Escape::Exit(3))) => {}
        other => panic!("expected exit 3, got {other:?}"),
    }
}

/// A cleanup's own failure pre-empts the halt it ran under.
#[test]
fn a_cleanups_fail_pre_empts_an_exit() {
    let mut shell = test_shell();
    match top_level(
        &mut shell,
        "guard { exit 3 } { fail [status: 7, message: x] }",
    ) {
        Err(Break::Error(e)) => assert_eq!(e.code(), 7),
        other => panic!("expected an error with code 7, got {other:?}"),
    }
}
