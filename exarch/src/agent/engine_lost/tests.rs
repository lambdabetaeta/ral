//! The sentence is the product here: one sentence, a start told apart from a
//! death, and both things a debugger needs.

use super::{EngineLost, EnginePhase};
use ral_core::carrier::Severed;
use ral_core::own::{OwnDir, TestDir};

fn closed() -> Severed {
    Severed::Closed("the engine closed the connection".into())
}

/// A run directory of this test's own, deleted with the guard.
fn run_dir() -> TestDir {
    OwnDir::for_test("engine-lost").expect("a run directory")
}

/// A start failure and a death must not read alike.
#[test]
fn a_start_failure_and_a_death_read_differently() {
    let starting = EngineLost::starting(&closed(), &run_dir()).to_string();
    let running = EngineLost::running(&closed(), &run_dir()).to_string();
    assert_ne!(starting, running);
}

/// A user's line carries somewhere to go and nothing else: no code and no
/// transport's restatement.
#[test]
fn the_sentence_carries_a_path_and_nothing_else() {
    let root = run_dir();
    let dir = root
        .make_dir("2026-09-16-170235-26184")
        .expect("a run directory");
    let shown = EngineLost::starting(&closed(), &dir).to_string();
    assert!(shown.contains("2026-09-16-170235-26184"), "{shown}");
    assert!(
        !shown.contains("engine-closed"),
        "a code is for the log, not the window: {shown}"
    );
    assert!(
        !shown.contains("the engine closed the connection"),
        "the transport's own restatement is what this replaced: {shown}"
    );
    assert_eq!(
        shown.lines().count(),
        1,
        "what the user is shown is one line: {shown}"
    );
}

/// What goes into a record keeps the engine's own account of itself; what
/// goes into a window does not.  The split is the whole design.
#[test]
fn the_logged_form_keeps_what_the_sentence_drops() {
    let refused = ral_core::error::Error::new("protocol version 9").with_hint("upgrade the engine");
    let lost = EngineLost::running(
        &Severed::Refused(ral_core::protocol::WireError::of(&refused)),
        &run_dir(),
    );
    let logged = lost.logged();
    assert!(logged.starts_with(&lost.to_string()), "{logged}");
    assert!(
        logged.contains("protocol version 9"),
        "the engine's own refusal is worth keeping somewhere: {logged}"
    );
    assert!(logged.contains("hint: upgrade the engine"), "{logged}");
    assert!(
        logged.contains("engine-refused") && !lost.to_string().contains("engine-refused"),
        "the code left the window for the record, not the bin: {logged}"
    );
    assert_eq!(lost.phase(), EnginePhase::Running);
    assert_eq!(lost.cause().code(), "engine-refused");
}
