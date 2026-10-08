use super::*;
use crate::provider::{Limit, Refusal};
use crate::record::AgentId;

fn fresh_log() -> AgentLog {
    AgentLog::for_test(
        AgentId::new(0),
        "test",
        &crate::record::RecordedAccount::for_test("test"),
    )
    .expect("session log")
}

fn facts() -> Facts {
    Facts {
        must_reply: false,
        pinned: None,
        quiet: true,
    }
}

const COMPLETE: Result<&Outcome, &ProviderError> = Ok(&Outcome::Complete);
const EMPTY: Result<&Outcome, &ProviderError> = Ok(&Outcome::Empty);

/// The transient exhausted the provider loop before the model produced output;
/// nudging would resend that same invisible request as a fake user turn.
#[test]
fn transient_surfaces_without_nudge() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    let error = ProviderError::Transient {
        cause: "stream idle: no response within timeout".into(),
        attempts: 3,
        body: None,
        status: None,
    };
    assert!(
        nudges.react(Err(&error), &facts(), &mut log).is_none(),
        "transient provider failures are surfaced directly"
    );
    assert_eq!(nudges.used, 0);
}

/// The provider loop owns retry attempts; the budget here is only for
/// model-visible recovery.
#[test]
fn provider_failures_do_not_consume_budget() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    let error = ProviderError::Refused(Refusal::for_test(Limit::Rate, None));
    for _ in 0..=BUDGET {
        assert!(
            nudges.react(Err(&error), &facts(), &mut log).is_none(),
            "provider failure should stop, not nudge"
        );
    }
    assert_eq!(nudges.used, 0);
}

#[test]
fn empty_turn_nudges_and_consumes_budget() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    match nudges.react(EMPTY, &facts(), &mut log) {
        Some(msg) => assert!(msg.contains("no text and no tool calls")),
        None => panic!("empty turn must nudge, not stop"),
    }
    assert_eq!(nudges.used, 1);
}

#[test]
fn exhausted_budget_stops() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    for _ in 0..BUDGET {
        assert!(nudges.react(EMPTY, &facts(), &mut log).is_some());
    }
    assert!(nudges.react(EMPTY, &facts(), &mut log).is_none());
}

/// A toolless trunk has nothing to be steered toward: no repair, no
/// reminder, nothing synthetic joins its conversation.
#[test]
fn a_trunk_that_steers_nothing_says_nothing() {
    let mut nudges = Nudges::new(false);
    let mut log = fresh_log();
    assert!(nudges.react(EMPTY, &facts(), &mut log).is_none());
    let pressure = Reminder::Pressure {
        detail: "400 of 500 tokens".into(),
        planned: None,
    };
    assert!(nudges.remind(&pressure, &mut log).is_none());
    assert_eq!(nudges.used, 0);
}

/// Once the budget is spent the un-replied finish is accepted (`None`) so the
/// loop ends; `agent_outcome` in `agent/attend.rs` maps that `Complete` to
/// `Failed`.
#[test]
fn no_reply_finish_re_nudges_up_to_budget_then_fails() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    let f = || Facts {
        must_reply: true,
        ..facts()
    };
    for _ in 0..BUDGET {
        match nudges.react(COMPLETE, &f(), &mut log) {
            Some(msg) => assert!(msg.contains("`reply`")),
            None => {
                panic!("a returning agent that did not reply must re-nudge while budget remains")
            }
        }
    }
    assert_eq!(
        nudges.used, BUDGET,
        "the no-reply nudge now draws on the budget"
    );
    assert!(
        nudges.react(COMPLETE, &f(), &mut log).is_none(),
        "past the budget the un-replied finish is accepted (mapped to Failed downstream)"
    );
}

/// A conversing agent never returns, so it owes no `reply`.
#[test]
fn root_completion_is_never_reply_nudged() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    assert!(nudges.react(COMPLETE, &facts(), &mut log).is_none());
}

/// A live child *is* the next action, so every nudge — the pin reminder
/// included — waits for it to settle rather than pile on; and once it
/// settles the pin reminder fires on the first quiet completion.
#[test]
fn pinned_state_waits_while_children_live() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    let busy = Facts {
        pinned: Some("tasks 3/8".into()),
        quiet: false,
        ..facts()
    };
    assert!(
        nudges.react(COMPLETE, &busy, &mut log).is_none(),
        "a live descendant is the next action, so the pin reminder should wait"
    );
    assert_eq!(nudges.used, 0, "waiting must not spend nudge budget");

    let quiet = Facts {
        pinned: Some("tasks 3/8".into()),
        quiet: true,
        ..facts()
    };
    let msg = nudges
        .react(COMPLETE, &quiet, &mut log)
        .expect("the first quiet completion after settling should nudge");
    assert!(msg.contains("There is pinned state: tasks 3/8"));
}

/// Both obligations compose into one reminder, each on its own accounting:
/// the reply half spends budget, the pinned-state half does not.
#[test]
fn returning_agent_gets_both_reply_and_pinned_nudges() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    let f = Facts {
        must_reply: true,
        pinned: Some("tasks 3/8".into()),
        ..facts()
    };
    let msg = nudges
        .react(COMPLETE, &f, &mut log)
        .expect("a returning agent with pinned state should nudge");
    assert!(
        msg.contains("`reply`"),
        "must still be reminded to reply: {msg}"
    );
    assert!(
        msg.contains("There is pinned state: tasks 3/8"),
        "must also be reminded of its pinned state: {msg}"
    );
    assert_eq!(nudges.used, 1, "only the reply half spends budget");
}

#[test]
fn reset_clears_budget() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    let _ = nudges.react(EMPTY, &facts(), &mut log);
    assert!(nudges.used >= 1);
    nudges.reset();
    assert_eq!(nudges.used, 0, "reset clears the budget");
}

/// `Replied` answers `None` for peer and headless-root facts alike, budget
/// untouched — there is no self-verification round-trip left.
#[test]
fn any_reply_is_accepted_outright() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    for must_reply in [false, true] {
        let f = Facts {
            must_reply,
            ..facts()
        };
        assert!(nudges.react(Ok(&Outcome::Replied), &f, &mut log).is_none());
    }
    assert_eq!(nudges.used, 0);
}

/// The livelock regression: the same digest over repeated quiet
/// completions fires exactly once; a changed digest fires again; and
/// unpinning then re-pinning the identical digest fires again too.
#[test]
fn pin_reminder_fires_once_per_register_change() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    let with = |digest: &str| Facts {
        pinned: Some(digest.into()),
        ..facts()
    };

    let first = nudges
        .react(COMPLETE, &with("tasks 3/8"), &mut log)
        .expect("the first sighting of a digest must nudge");
    assert!(first.contains("tasks 3/8"));
    for _ in 0..3 {
        assert!(
            nudges
                .react(COMPLETE, &with("tasks 3/8"), &mut log)
                .is_none(),
            "a stationary digest must not re-fire"
        );
    }

    let changed = nudges
        .react(COMPLETE, &with("tasks 4/8"), &mut log)
        .expect("a changed digest must fire again");
    assert!(changed.contains("tasks 4/8"));

    // Unpin, then re-pin the identical digest: the edge re-arms through
    // `None`.
    let none = Facts {
        pinned: None,
        ..facts()
    };
    assert!(nudges.react(COMPLETE, &none, &mut log).is_none());
    let refired = nudges
        .react(COMPLETE, &with("tasks 4/8"), &mut log)
        .expect("re-pinning even the identical digest after an empty register must fire");
    assert!(refired.contains("tasks 4/8"));
}

/// A must-reply completion at budget with a pin due returns `None` and
/// leaves that edge armed.
#[test]
fn exhausted_reply_budget_leaves_the_pin_edge_armed() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    let must_reply = Facts {
        must_reply: true,
        ..facts()
    };
    for _ in 0..BUDGET {
        assert!(nudges.react(COMPLETE, &must_reply, &mut log).is_some());
    }
    let pin_due = Facts {
        must_reply: true,
        pinned: Some("tasks 3/8".into()),
        ..facts()
    };
    assert!(
        nudges.react(COMPLETE, &pin_due, &mut log).is_none(),
        "past budget the un-replied finish is accepted"
    );

    nudges.reset();
    let no_must_reply = Facts {
        pinned: Some("tasks 3/8".into()),
        ..facts()
    };
    let msg = nudges
        .react(COMPLETE, &no_must_reply, &mut log)
        .expect("after reset the pin edge must still be armed");
    assert!(msg.contains("There is pinned state: tasks 3/8"), "{msg}");
}

/// `reset()` clears only the budget, not the edges; the rebirth clears
/// everything.
#[test]
fn reset_does_not_rearm_edges() {
    let mut nudges = Nudges::new(true);
    let mut log = fresh_log();
    let f = Facts {
        pinned: Some("tasks 3/8".into()),
        ..facts()
    };
    assert!(nudges.react(COMPLETE, &f, &mut log).is_some());
    nudges.reset();
    assert!(
        nudges.react(COMPLETE, &f, &mut log).is_none(),
        "reset must not re-arm a told edge"
    );

    nudges = nudges.reborn();
    assert!(
        nudges.react(COMPLETE, &f, &mut log).is_some(),
        "the rebirth must fire again"
    );
}

/// The pressure reminder carries both the reading and the turns the next
/// boundary would cut, with the line that makes the same cut by hand.
#[test]
fn pressure_names_the_cut_and_offers_the_note() {
    let body = Reminder::Pressure {
        detail: "400 of 500 tokens".into(),
        planned: Some(vec![1, 2, 3, 4, 5, 6, 7]),
    }
    .body();
    assert!(body.contains("400 of 500 tokens"), "{body}");
    assert!(
        body.contains("turns 1–7 will leave your context")
            && body.contains("`exarch-context `evict [turns: !{range 1 8}"),
        "must name the cut and offer the note: {body}"
    );
}

/// Nothing old enough to shed: the reading stands alone, with no cut to
/// announce and no note to offer.
#[test]
fn pressure_without_a_planned_cut_states_the_reading_alone() {
    let body = Reminder::Pressure {
        detail: "400 of 500 tokens".into(),
        planned: None,
    }
    .body();
    assert!(body.contains("400 of 500 tokens"), "{body}");
    assert!(
        !body.contains("evict") && !body.contains("will leave your context"),
        "with no cut planned there is nothing to announce: {body}"
    );
}

/// A reminder records its breadcrumb spending nothing: the budget is the
/// repairs' alone.
#[test]
fn a_reminder_spends_no_budget() {
    let nudges = Nudges::new(true);
    let mut log = fresh_log();
    let reminder = Reminder::Ration {
        service: "openrouter".into(),
        label: "5 hours".into(),
        pct: 91,
        windowed: true,
        resumes: true,
    };
    let text = nudges
        .remind(&reminder, &mut log)
        .expect("a steering trunk is reminded");
    assert!(
        text.contains("91% spent") && text.contains("pauses until it resets"),
        "{text}"
    );
    assert_eq!(nudges.used, 0);
}
