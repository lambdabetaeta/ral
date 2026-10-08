use super::*;
use ral_core::types::{AuditIo, CommandOrigin, Resource};

/// The card's first [`Mark::Text`] flattened — the on-screen line, roling
/// dropped.
fn line(card: &Card) -> String {
    let Card(marks) = card;
    match &marks[0] {
        Mark::Text { spans } => spans.iter().map(|s| s.text.as_str()).collect(),
        _ => panic!("expected a text mark"),
    }
}

fn command(argv: &[&str], status: i32) -> Observed {
    let (shown, args) = argv.split_first().expect("a command names a program");
    Observed::command(
        shown,
        args.iter().map(ToString::to_string),
        status,
        CommandOrigin::External,
        AuditIo::default(),
        None,
    )
}

#[test]
fn exec_requotes_only_where_the_shell_would_reparse() {
    let cmd = |argv: &[&str]| -> String {
        let full = line(&observation_card(&command(argv, 0)));
        full.strip_prefix("$ ")
            .and_then(|s| s.strip_suffix(" → 0"))
            .expect("the `$ … → status` frame")
            .to_string()
    };
    // Per token, not per line: nothing shell-safe gains quotes it lacked.
    assert_eq!(
        cmd(&["grep", "-n", "why-the-ubuntu-22-fiction", "VM.md"]),
        "grep -n why-the-ubuntu-22-fiction VM.md"
    );
    assert_eq!(cmd(&["ls", "README.md"]), "ls README.md");
    // Whatever quoting shlex picks, the line word-splits back to the argv.
    let tricky = ["echo", "hello world", "*.rs", "it's", ""];
    assert_eq!(
        shlex::split(&cmd(&tricky)).expect("the rendered line re-parses"),
        tricky.map(String::from)
    );
}

#[test]
fn capability_card_denies_role_bad_and_shows_its_fields() {
    let card = observation_card(&Observed::Check(Check::new(
        Resource::Fs,
        [
            ("op".to_string(), "write".to_string()),
            ("path".to_string(), "/etc/passwd".to_string()),
        ]
        .into_iter()
        .collect(),
    )));
    assert_eq!(line(&card), "check fs denied op=write path=/etc/passwd");
    let Card(marks) = &card;
    let Mark::Text { spans } = &marks[0] else {
        panic!("expected a text mark")
    };
    let denied = spans
        .iter()
        .find(|s| s.text == "denied")
        .expect("a decision span");
    assert_eq!(denied.role, Some(Role::Bad));
}
