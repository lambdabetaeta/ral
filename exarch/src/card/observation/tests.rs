use super::*;
use ral_core::fact::{Act, AuditIo, Write};
use ral_core::shell::runtime::{CommandOrigin, Resource, WriteOutcome};

/// The card's first [`Mark::Text`] flattened — the on-screen line, roling
/// dropped.
fn line(card: &Card) -> String {
    let Card(marks) = card;
    match &marks[0] {
        Mark::Text { spans } => spans.iter().map(|s| s.text.as_str()).collect(),
        _ => panic!("expected a text mark"),
    }
}

fn fact(argv: &[&str], status: u8) -> Command {
    let (shown, args) = argv.split_first().expect("a command names a program");
    Command::new(
        shown,
        args.iter().map(ToString::to_string),
        status,
        CommandOrigin::External,
        AuditIo::default(),
        None,
    )
}

fn command(argv: &[&str], status: u8) -> Landed {
    Landed::Effect(Effect::Command(fact(argv, status)))
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
    let card = observation_card(&Landed::Surfaced(Check::new(
        Resource::Fs,
        [
            ("op".to_string(), "write".to_string()),
            ("path".to_string(), "/etc/passwd".to_string()),
        ]
        .into_iter()
        .collect(),
    )));
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

fn check(resource: Resource) -> Check {
    Check::new(resource, BTreeMap::new())
}

/// A flagged deputy check is no denial; a write draws as its change, an act is the desk's.
#[test]
fn quiet_observations_land_nowhere() {
    let write = Write {
        path: "a".into(),
        mode: ral_core::ir::WriteMode::Write,
        outcome: WriteOutcome::Committed,
        new_bytes: None,
        old_bytes: None,
    };
    let act = Act {
        verb: "v".into(),
        subject: None,
        payload: String::new(),
        refused: false,
    };
    for what in [
        Observed::Check(check(Resource::Deputy)),
        Observed::Write(write),
        Observed::Act(act),
    ] {
        assert!(Landed::of(&what).is_none());
    }
}
