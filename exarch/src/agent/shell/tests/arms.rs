//! `Agent::ral`'s result arms: each failure settles to `failed`, and only a
//! run that reached evaluation reports an exit.

use super::*;

fn exit_line(text: &str) -> Option<&str> {
    text.lines().find(|line| line.starts_with("EXIT: "))
}

fn run(session: &mut Agent, cmd: &str, timeout_secs: u64) -> Evaluated {
    let (tx, _rx) = crate::bus::channel();
    let emit = Emitter::new(tx, session.id());
    session.ral(&DurableRoot::default().child(), cmd, timeout_secs, &emit)
}

#[test]
fn a_static_rejection_fails_without_an_exit_and_leaves_the_shell_usable() {
    let mut session = Agent::for_test("system").unwrap();
    let rejected = run(&mut session, "let = = =", 5);
    assert!(rejected.failed);
    assert_eq!(exit_line(&rejected.text), None, "{}", rejected.text);
    let after = run(&mut session, "return 1", 5);
    assert!(!after.failed);
    assert_eq!(exit_line(&after.text), Some("EXIT: 0"));
}

#[test]
fn a_panicked_run_reports_exit_101_and_leaves_the_shell_usable() {
    let mut session = dressed_trunk(|runtime| runtime.install_builtins(PANIC_BUILTINS));
    let panicked = run(&mut session, "a4-panic-now", 5);
    assert!(panicked.failed);
    assert_eq!(
        exit_line(&panicked.text),
        Some("EXIT: 101"),
        "{}",
        panicked.text
    );
    let after = run(&mut session, "return 1", 5);
    assert!(!after.failed);
    assert_eq!(exit_line(&after.text), Some("EXIT: 0"), "{}", after.text);
}
