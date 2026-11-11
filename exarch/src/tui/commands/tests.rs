use super::limits_card;
use super::{
    Command, cmd_export, command_candidates, lookup_command, route_submit, unrecognized_command,
};
use crate::card::Mark;
use crate::provider::allowance::{
    Allowance, BudgetKind, BudgetReset, Consumption, Reading, Scope, Unit,
};
use crate::record::{Born, Emitter as Recorder, Started, Transcript, Voice};
use crate::tui::app::tests::{app, app_born, ctx, text};
use ral_core::own::OwnDir;
use std::time::Duration;

fn replacements(line: &str) -> Vec<String> {
    command_candidates(line)
        .into_iter()
        .map(|c| c.replacement)
        .collect()
}

fn dispatch(input: &str) -> Option<(&'static str, String)> {
    lookup_command(input).map(|(spec, arg)| (spec.name, arg.to_string()))
}

fn parse(input: &str) -> Option<Command> {
    lookup_command(input).map(|(spec, arg)| (spec.command)(arg))
}

/// What submitting `line` leaves on the focused tab.
fn submitted(line: &str) -> String {
    let (mut app, _rx, root) = app();
    assert!(route_submit(line.into(), &mut app, &ctx()).is_continue());
    text(&mut app, root.id())
}

#[test]
fn argless_command_matches_alone_but_not_with_trailing_text() {
    assert_eq!(dispatch("/copy"), Some(("/copy", String::new())));
    assert_eq!(dispatch("/copy this"), None);
    assert_eq!(dispatch("/exit"), Some(("/quit", String::new())));
    assert_eq!(dispatch("/resources"), Some(("/resources", String::new())));
    assert_eq!(dispatch("/context"), Some(("/context", String::new())));
    assert_eq!(parse("/rewind"), Some(Command::Rewind));
    assert_eq!(dispatch("/rewind 7"), None);
}

#[test]
fn export_consumes_its_path_argument() {
    assert_eq!(
        dispatch("/export ~/notes.md"),
        Some(("/export", "~/notes.md".to_string()))
    );
    assert_eq!(
        dispatch("/export   /tmp/a.txt  "),
        Some(("/export", "/tmp/a.txt".to_string()))
    );
    // A bare command matches; routing turns the empty argument into the
    // usage hint rather than letting the line fall through to the model.
    assert_eq!(dispatch("/export"), Some(("/export", String::new())));
    assert!(submitted("/export").contains("usage: /export <path>"));
}

#[test]
fn unrecognized_command_flags_only_a_slash_typo() {
    assert_eq!(unrecognized_command("/bogus"), Some("/bogus"));
    assert_eq!(
        unrecognized_command("/bad_command here are the argv"),
        Some("/bad_command")
    );
    // A real command misused with trailing text is a deliberate fall-through
    // to the model, not a typo.
    assert_eq!(unrecognized_command("/copy this"), None);
    assert_eq!(unrecognized_command("just a prompt"), None);
}

#[test]
fn a_typed_space_or_a_plain_line_ends_the_completion() {
    assert_eq!(replacements("/export "), Vec::<String>::new());
    assert_eq!(replacements("/export ~/notes.md"), Vec::<String>::new());
    assert_eq!(replacements("what is a monad"), Vec::<String>::new());
}

#[test]
fn sort_orders_five_hour_before_weekly() {
    let weekly = Allowance {
        scope: Scope::Window(Some(Duration::from_hours(7 * 24))),
        used: Consumption::Fraction(0.2),
        resets_at: None,
    };
    let five_hour = Allowance {
        scope: Scope::Window(Some(Duration::from_hours(5))),
        used: Consumption::Fraction(0.5),
        resets_at: None,
    };
    let card = limits_card(&[(
        "anthropic".to_string(),
        Reading::Allowances(vec![weekly, five_hour]),
    )]);
    let Mark::Fields { rows } = &card.0[2] else {
        panic!("an allowances reading renders one fields mark");
    };
    assert!(rows[0].label.contains("5 hours"));
    assert!(rows[1].label.contains("7 days"));
}

#[test]
fn limits_renders_peer_accounts_exact_money_and_explicit_percentages() {
    let card = limits_card(&[
        (
            "openrouter".into(),
            Reading::Allowances(vec![Allowance {
                scope: Scope::Budget {
                    kind: BudgetKind::Key,
                    reset: Some(BudgetReset::Monthly),
                },
                used: Consumption::Counted {
                    used: 2000,
                    limit: Some(10000),
                    unit: Unit::Dollars,
                },
                resets_at: None,
            }]),
        ),
        (
            "openai-codex · person (workspace)".into(),
            Reading::Allowances(vec![Allowance {
                scope: Scope::Window(Some(Duration::from_hours(168))),
                used: Consumption::Fraction(0.0),
                resets_at: None,
            }]),
        ),
    ]);
    for width in [40, 100] {
        let lines =
            crate::tui::line::render_card_framed(&card, 2, width, crate::tui::dial::Detail::Full);
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.lines().nth(1).unwrap().contains("limits"), "{text}");
        assert!(
            !text.lines().nth(1).unwrap().contains("openrouter"),
            "{text}"
        );
        assert!(text.contains("openrouter"), "{text}");
        assert!(text.contains("openai-codex"), "{text}");
        assert!(text.contains("$20.00 used"), "{text}");
        assert!(text.contains("$80.00"), "{text}");
        assert!(text.contains("remaining"), "{text}");
        assert!(text.contains("0% used"), "{text}");
        assert!(!text.contains("0/100"), "{text}");
        assert!(
            lines.iter().all(|line| line.width() <= usize::from(width)),
            "{text}"
        );
    }
}

/// `/export` writes a new file only; the one already there is left alone.
#[test]
fn export_refuses_to_overwrite() {
    let dir = OwnDir::for_test("export-overwrite").expect("a test directory");
    let recorder = Recorder::create(&dir).unwrap();
    let born = Born {
        log_dir: OwnDir::clone(&dir),
        name: "main".into(),
        parent: None,
        resumed: None,
        reader: recorder.reader(),
    };
    let (mut app, _rx, root) = app_born(&born);
    recorder
        .begin(Started {
            session_id: root.id(),
            name: "main".into(),
            parent: None,
            returns: false,
            selected: crate::provider::Selected::for_test("test"),
            log_dir: dir.path().to_path_buf(),
            at_unix_ms: 0,
        })
        .unwrap();
    let _ = recorder
        .emit(Transcript::UserPrompt {
            turn: 1,
            text: "exportable prompt".into(),
            voice: Voice::Human,
        })
        .unwrap();
    recorder.sync().unwrap();
    let fresh = dir.path().join("fresh.txt");
    cmd_export(&mut app, &ral_core::path::shown(&fresh), ctx().info);
    assert!(
        std::fs::read_to_string(&fresh)
            .unwrap()
            .contains("exportable prompt")
    );
    let dest = dir.path().join("out.txt");
    std::fs::write(&dest, "keep me").expect("a file to protect");
    cmd_export(&mut app, &ral_core::path::shown(&dest), ctx().info);
    assert_eq!(std::fs::read_to_string(&dest).expect("it reads"), "keep me");
    // The card wraps at the window's width, and the path it names is long.
    let all = text(&mut app, root.id())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        all.contains("already exists, and is never overwritten: choose another name"),
        "{all:?}"
    );
}
