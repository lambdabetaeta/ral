use super::{EngineLost, EnginePhase};
use ral_core::carrier::Severed;

fn closed() -> Severed {
    Severed::Closed("the engine closed the connection".into())
}

/// The bug this replaced: a session that never started was told to start
/// a new one.  The two phases must not read alike.
#[test]
fn a_start_failure_and_a_death_give_different_advice() {
    let starting = EngineLost::starting(&closed(), None).to_string();
    let running = EngineLost::running(&closed(), None).to_string();
    assert_ne!(starting, running);
    assert!(
        !starting.to_lowercase().contains("start a new"),
        "a session that never began cannot be told to begin another: {starting}"
    );
    assert!(
        running.to_lowercase().contains("start a new"),
        "a session whose state is gone is worth abandoning: {running}"
    );
}

/// A refusal is deterministic, so a refused start is not told to retry.
#[test]
fn a_refused_start_is_not_told_to_try_again() {
    let shown =
        EngineLost::starting(&Severed::Refused("protocol version 9".into()), None).to_string();
    assert!(!shown.to_lowercase().contains("try again"), "{shown}");
}

/// A user's line carries somewhere to go and nothing else: no code, no
/// transport's restatement, and no dash holding the advice at arm's
/// length.
#[test]
fn the_sentence_carries_a_path_and_nothing_else() {
    let dir = std::path::PathBuf::from("/state/synod/proj/2026-09-16-170235-26184");
    let shown = EngineLost::starting(&closed(), Some(&dir)).to_string();
    assert!(shown.contains("2026-09-16-170235-26184"), "{shown}");
    assert!(
        !shown.contains("engine-closed"),
        "a code is for the log, not the window: {shown}"
    );
    assert!(
        !shown.contains('—'),
        "the advice is its own sentence, not a clause after a dash: {shown}"
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

/// With nowhere to send a reader the invitation is simply absent — never
/// a dangling "details in", and never an empty bracket in its place.
#[test]
fn a_failure_with_no_log_is_the_sentence_alone() {
    let lost = EngineLost::running(&Severed::Faulted("junk frame".into()), None);
    let shown = lost.to_string();
    assert!(!shown.contains("details in"), "{shown}");
    assert!(!shown.contains('('), "{shown}");
    assert!(
        lost.logged().contains("engine-faulted"),
        "the code the window dropped is still in the record: {}",
        lost.logged()
    );
}

/// What goes into a record keeps the engine's own account of itself; what
/// goes into a window does not.  The split is the whole design.
#[test]
fn the_logged_form_keeps_what_the_sentence_drops() {
    let lost = EngineLost::running(&Severed::Refused("protocol version 9".into()), None);
    let logged = lost.logged();
    assert!(logged.starts_with(&lost.to_string()), "{logged}");
    assert!(
        logged.contains("protocol version 9"),
        "the engine's own refusal is worth keeping somewhere: {logged}"
    );
    assert!(
        logged.contains("engine-refused") && !lost.to_string().contains("engine-refused"),
        "the code left the window for the record, not the bin: {logged}"
    );
    assert_eq!(lost.phase(), EnginePhase::Running);
    assert_eq!(lost.cause().code(), "engine-refused");
}
