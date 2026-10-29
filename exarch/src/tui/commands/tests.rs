use super::limits_card;
use super::{
    Command, cmd_export, command_candidates, lookup_command, resolve_export_path, route_submit,
    unrecognized_command,
};
use crate::card::Mark;
use crate::provider::allowance::{
    Allowance, BudgetKind, BudgetReset, Consumption, Reading, Scope, Unit,
};
use crate::record::{Born, RECORD};
use crate::tui::app::tests::{app, app_born, ctx, text};
use ral_core::own::OwnDir;
use std::path::{Path, PathBuf};
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
    route_submit(line.into(), &mut app, &ctx());
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
fn stop_is_a_key_and_not_a_command() {
    assert_eq!(dispatch("/stop"), None);
}

#[test]
fn quit_ends_every_root_and_descendant_without_queuing() {
    use crate::agent::Trunk;
    use crate::agent::spawn::hatch_root;
    use crate::agent::testkit::trunk;
    use crate::record::AgentLog;
    use ral_core::process::CancelCause;

    let main = trunk(Trunk::Converses);
    let other = hatch_root(&main, crate::record::Memory::Amnemon, Some("other".into()))
        .expect("a second root");
    let child = main.fork_named("child").expect("a child");
    let grandchild = child.fork_named("grandchild").expect("a grandchild");
    let peer = other.fork_named("peer").expect("the other root's child");

    super::quit(&main.fleet);

    for agent in [&main, &other, &child, &grandchild, &peer] {
        let handle = agent.handle();
        assert!(
            !handle.is_live(),
            "quit ends {} without a command consumer",
            handle.0.name
        );
        assert_eq!(agent.mailbox.life().cause(), Some(CancelCause::Cancelled));
        assert!(
            agent
                .engine()
                .read(|t| t.session_ended())
                .unwrap()
                .is_none(),
            "quit cancels lives; the owner retires its engine"
        );
        assert!(
            handle.queued().is_empty(),
            "quit is shutdown, not queued input"
        );
    }
    assert_eq!(
        AgentLog::live_roots(&main.fleet.launch.sessions_root)
            .unwrap()
            .len(),
        2,
        "quit leaves both root sessions resumable"
    );
}

#[test]
fn quit_joins_a_resting_helper_even_when_its_parent_has_not_run() {
    use crate::agent::Trunk;
    use crate::agent::testkit::{launch_child, ral_call, scripted, set_provider, trunk};
    use crate::bus::{FleetBus, Signal};
    use crate::provider::scripted::{Reply, Script};
    use crate::record::{AgentState, ChildStatus, Transient};
    use ral_core::test_helper::eventually;

    let mut main = trunk(Trunk::Converses);
    let fleet = main.fleet.clone();
    let child = main.fork_named("child").unwrap();
    let id = child.id();
    set_provider(
        &child,
        scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![ral_call(
                "reply",
                "exarch-agents `reply ()",
            )])),
        ),
    );
    let bus = FleetBus::session();
    let emit = bus.emitter(main.id());
    let handle = launch_child(&mut main, child, Some("work".into()), &emit);
    assert!(
        eventually(Duration::from_secs(5), || {
            let status = handle.status();
            (status.waiting_for_input
                && status.job.is_none()
                && matches!(status.report, Some(ChildStatus::Replied(_))))
            .then_some(())
        })
        .is_some(),
        "the replied helper rests for follow-up"
    );
    assert!(std::iter::from_fn(|| bus.rx().try_recv().ok()).any(|signal| {
        matches!(signal, Signal::Transient(agent, Transient::State(AgentState::Ready)) if agent == id)
    }));
    super::quit(&fleet);
    drop(main);

    assert!(fleet.is_empty(), "dropping the owner joins its helper");
    assert!(!handle.is_live());
    assert_eq!(
        std::iter::from_fn(|| bus.rx().try_recv().ok())
            .filter(|signal| {
                matches!(signal, Signal::Transient(agent, Transient::Died { .. }) if *agent == id)
            })
            .count(),
        1,
        "the helper's final cue precedes its owner's completed drop"
    );
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
fn focus_consumes_its_name_argument() {
    assert_eq!(
        dispatch("/focus scout"),
        Some(("/focus", "scout".to_string()))
    );
    assert!(submitted("/focus").contains("usage: /focus <name>"));
}

#[test]
fn unknown_token_is_not_a_command() {
    assert_eq!(dispatch("/bogus"), None);
    assert_eq!(dispatch("just a prompt"), None);
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
fn a_prefix_narrows_and_an_alias_stands_for_itself() {
    assert_eq!(replacements("/thin"), ["/thinking"]);
    assert!(replacements("/ex").contains(&"/exit".to_string()));
}

#[test]
fn a_typed_space_or_a_plain_line_ends_the_completion() {
    assert_eq!(replacements("/export "), Vec::<String>::new());
    assert_eq!(replacements("/export ~/notes.md"), Vec::<String>::new());
    assert_eq!(replacements("what is a monad"), Vec::<String>::new());
}

#[test]
fn the_argument_hint_shows_but_is_never_spliced() {
    let export = command_candidates("/export")
        .into_iter()
        .find(|c| c.replacement == "/export")
        .expect("/export completes itself");
    assert_eq!(export.display, "/export <path>");
    assert_eq!(
        export.detail.as_deref(),
        Some("Write the user view to a file.")
    );
}

// Twins rather than one genericised test: absoluteness is host-defined
// (`/tmp/out.txt` is not absolute on Windows), so each host pins its own.
#[cfg(unix)]
#[test]
fn export_path_resolves_absolute_and_relative() {
    let cwd = &ral_core::path::CwdPath::new(PathBuf::from("/Users/me/proj"))
        .expect("an absolute fixture");
    assert_eq!(
        resolve_export_path("/tmp/out.txt", cwd),
        Path::new("/tmp/out.txt")
    );
    assert_eq!(
        resolve_export_path("notes.md", cwd),
        Path::new("/Users/me/proj/notes.md")
    );
}

#[cfg(windows)]
#[test]
fn export_path_resolves_absolute_and_relative() {
    let cwd = &ral_core::path::CwdPath::new(PathBuf::from(r"C:\Users\me\proj"))
        .expect("an absolute fixture");
    assert_eq!(
        resolve_export_path(r"C:\scratch\out.txt", cwd),
        Path::new(r"C:\scratch\out.txt")
    );
    assert_eq!(
        resolve_export_path("notes.md", cwd),
        Path::new(r"C:\Users\me\proj\notes.md")
    );
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
fn all_unmetered_collapses_to_one_sentence_naming_the_accounts() {
    let card = limits_card(&[
        ("anthropic".to_string(), Reading::Unmetered),
        ("openai".to_string(), Reading::Unmetered),
        ("deepseek".to_string(), Reading::Unmetered),
    ]);
    assert_eq!(card.0.len(), 2);
    let Mark::Text { spans } = &card.0[1] else {
        panic!("the collapsed card has one sentence below its title");
    };
    assert!(spans[0].text.contains("anthropic, openai, deepseek"));
    assert!(spans[0].text.contains("publishes a ration"));
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
    // A record to export, so that it is the destination that refuses.
    dir.append(RECORD).expect("an empty record");
    let born = Born {
        log_dir: OwnDir::clone(&dir),
        ..Born::for_test("main", None)
    };
    let (mut app, _rx, root) = app_born(&born);
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
