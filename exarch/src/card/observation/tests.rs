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

fn fact(argv: &[&str], status: i32) -> Command {
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

fn command(argv: &[&str], status: i32) -> Landed {
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

#[test]
fn errand_birth_names_the_identity_and_title() {
    let text = worker_spans(WorkerId(7), "building")
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert_eq!(text, "errand #7 started · building");
}

fn check(resource: Resource) -> Check {
    Check::new(resource, BTreeMap::new())
}

#[test]
fn only_effects_denials_and_births_land() {
    assert!(matches!(
        Landed::of(&Observed::Command(fact(&["ls"], 0))),
        Some(Landed::Effect(Effect::Command(_)))
    ));
    assert!(matches!(
        Landed::of(&Observed::Read(Read { path: "a".into() })),
        Some(Landed::Effect(Effect::Read(_)))
    ));
    assert!(matches!(
        Landed::of(&Observed::Grep(Grep {
            scope: "src".into(),
            pattern: "x".into()
        })),
        Some(Landed::Effect(Effect::Grep(_)))
    ));
    assert!(matches!(
        Landed::of(&Observed::Worker(Worker {
            id: WorkerId(1),
            title: "t".into()
        })),
        Some(Landed::Announced(_))
    ));
    assert!(matches!(
        Landed::of(&Observed::Check(check(Resource::Fs))),
        Some(Landed::Surfaced(_))
    ));
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
