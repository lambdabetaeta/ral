use super::limits_card;
use super::{
    Command, Verb, command_candidates, lookup_command, resolve_export_path, unrecognized_command,
};
use crate::card::Mark;
use crate::provider::allowance::{Allowance, Consumption, Reading};
use std::time::Duration;

fn replacements(line: &str) -> Vec<String> {
    command_candidates(line)
        .into_iter()
        .map(|c| c.replacement)
        .collect()
}

fn dispatch(input: &str) -> Option<(&'static str, String)> {
    lookup_command(input).map(|(v, arg)| (v.meta().name, arg.to_string()))
}

fn parse(input: &str) -> Option<Result<Command, String>> {
    lookup_command(input).map(|(v, arg)| v.parse(arg))
}

#[test]
fn argless_command_matches_alone_but_not_with_trailing_text() {
    assert_eq!(dispatch("/copy"), Some(("/copy", String::new())));
    assert_eq!(dispatch("/copy this"), None);
    assert_eq!(dispatch("/exit"), Some(("/quit", String::new())));
    assert_eq!(dispatch("/resources"), Some(("/resources", String::new())));
    assert_eq!(dispatch("/context"), Some(("/context", String::new())));
    assert_eq!(parse("/rewind"), Some(Ok(Command::Rewind)));
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
    // A bare command matches; the parse turns the empty argument into the
    // usage hint rather than letting the line fall through to the model.
    assert_eq!(dispatch("/export"), Some(("/export", String::new())));
    assert!(matches!(parse("/export"), Some(Err(usage)) if usage.starts_with("usage:")));
}

#[test]
fn focus_consumes_its_name_argument() {
    assert_eq!(
        dispatch("/focus scout"),
        Some(("/focus", "scout".to_string()))
    );
    assert!(matches!(parse("/focus"), Some(Err(usage)) if usage.starts_with("usage:")));
}

#[test]
fn branch_matches_bare_and_with_prompt_and_close_resolves() {
    // An optional argument admits trailing text an argless one declines.
    assert_eq!(parse("/branch"), Some(Ok(Command::Branch(None))));
    assert_eq!(
        parse("/branch hi"),
        Some(Ok(Command::Branch(Some("hi".to_string()))))
    );
    assert_eq!(dispatch("/close"), Some(("/close", String::new())));
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
fn a_bare_slash_offers_every_command_and_alias() {
    let all: usize = Verb::ALL.iter().map(|v| v.meta().aliases.len() + 1).sum();
    assert_eq!(replacements("/").len(), all);
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
    assert_eq!(
        resolve_export_path("/tmp/out.txt", "/Users/me/proj").to_str(),
        Some("/tmp/out.txt")
    );
    assert_eq!(
        resolve_export_path("notes.md", "/Users/me/proj").to_str(),
        Some("/Users/me/proj/notes.md")
    );
}

#[cfg(windows)]
#[test]
fn export_path_resolves_absolute_and_relative() {
    assert_eq!(
        resolve_export_path(r"C:\scratch\out.txt", r"C:\Users\me\proj").to_str(),
        Some(r"C:\scratch\out.txt")
    );
    assert_eq!(
        resolve_export_path("notes.md", r"C:\Users\me\proj").to_str(),
        Some(r"C:\Users\me\proj\notes.md")
    );
}

#[test]
fn sort_orders_five_hour_before_weekly() {
    let weekly = Allowance {
        window: Some(Duration::from_hours(7 * 24)),
        used: Consumption::Fraction(0.2),
        resets_at: None,
    };
    let five_hour = Allowance {
        window: Some(Duration::from_hours(5)),
        used: Consumption::Fraction(0.5),
        resets_at: None,
    };
    let card = limits_card(&[(
        "anthropic".to_string(),
        Reading::Allowances(vec![weekly, five_hour]),
    )]);
    let Mark::Fields { rows } = &card.0[1] else {
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
    assert_eq!(card.0.len(), 1);
    let Mark::Text { spans } = &card.0[0] else {
        panic!("the collapsed card is one text mark");
    };
    assert!(spans[0].text.contains("anthropic, openai, deepseek"));
    assert!(spans[0].text.contains("publishes a ration"));
}

#[test]
fn empty_readings_say_something_sensible() {
    let card = limits_card(&[]);
    assert_eq!(card.0.len(), 1);
}
