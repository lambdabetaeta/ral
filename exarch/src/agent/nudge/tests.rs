use super::*;
use crate::provider::{Limit, Refusal};
use crate::record::AgentId;

fn fresh_log() -> AgentLog {
    AgentLog::for_test(AgentId::new(0), "test")
        .expect("session log")
        .0
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
    let log = fresh_log();
    let error = ProviderError::Transient {
        cause: "model response headers timed out".into(),
        attempts: 3,
        body: None,
        status: None,
    };
    assert!(
        nudges.react(Err(&error), &facts(), &log).is_none(),
        "transient provider failures are surfaced directly"
    );
    assert_eq!(nudges.used, 0);
}

/// The provider loop owns retry attempts; the budget here is only for
/// model-visible recovery.
#[test]
fn provider_failures_do_not_consume_budget() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let error = ProviderError::Refused(Refusal::for_test(Limit::Rate, None));
    for _ in 0..=BUDGET {
        assert!(
            nudges.react(Err(&error), &facts(), &log).is_none(),
            "provider failure should stop, not nudge"
        );
    }
    assert_eq!(nudges.used, 0);
}

#[test]
fn empty_turn_nudges_and_consumes_budget() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    match nudges.react(EMPTY, &facts(), &log) {
        Some(msg) => assert!(msg.contains("no text and no tool calls")),
        None => panic!("empty turn must nudge, not stop"),
    }
    assert_eq!(nudges.used, 1);
}

#[test]
fn exhausted_budget_stops() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    for _ in 0..BUDGET {
        assert!(nudges.react(EMPTY, &facts(), &log).is_some());
    }
    assert!(nudges.react(EMPTY, &facts(), &log).is_none());
}

/// A stall is not an output cap: the model is told the connection failed.
#[test]
fn a_stalled_truncation_gets_its_own_message_and_cause() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let error = ProviderError::Truncated {
        cause: Box::new(CutShort::Stalled(ProviderError::local_transient(
            "model response body read timed out",
        ))),
    };
    assert_eq!(
        nudges.react(Err(&error), &facts(), &log),
        Some(wrap_reminder(STALLED_MESSAGE))
    );
    assert_eq!(nudges.used, 1);
}

#[test]
fn reset_restores_the_spent_budget() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    for _ in 0..BUDGET {
        assert!(nudges.react(EMPTY, &facts(), &log).is_some());
    }
    assert!(nudges.react(EMPTY, &facts(), &log).is_none());
    nudges.reset();
    assert!(nudges.react(EMPTY, &facts(), &log).is_some());
}

/// A toolless trunk has nothing to be steered toward: no repair, no
/// reminder, nothing synthetic joins its conversation.
#[test]
fn a_trunk_that_steers_nothing_says_nothing() {
    let mut nudges = Nudges::new(false);
    let log = fresh_log();
    assert!(nudges.react(EMPTY, &facts(), &log).is_none());
    let pressure = Reminder::Pressure(Load {
        used: 400,
        total: Some(500),
        trigger: 450,
        unit: Unit::Tokens,
        planned: None,
    });
    assert!(nudges.remind(&pressure, &log).is_none());
    assert_eq!(nudges.used, 0);
}

/// Once the budget is spent the un-replied finish is accepted (`None`) so the
/// loop ends; `agent_outcome` in `agent/attend.rs` maps that `Complete` to
/// `Failed`.
#[test]
fn no_reply_finish_re_nudges_up_to_budget_then_fails() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let f = || Facts {
        must_reply: true,
        ..facts()
    };
    for _ in 0..BUDGET {
        match nudges.react(COMPLETE, &f(), &log) {
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
        nudges.react(COMPLETE, &f(), &log).is_none(),
        "past the budget the un-replied finish is accepted (mapped to Failed downstream)"
    );
}

/// A conversing agent never returns, so it owes no `reply`.
#[test]
fn root_completion_is_never_reply_nudged() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    assert!(nudges.react(COMPLETE, &facts(), &log).is_none());
}

/// A live child *is* the next action, so every nudge — the pin reminder
/// included — waits for it to settle rather than pile on; and once it
/// settles the pin reminder fires on the first quiet completion.
#[test]
fn pinned_state_waits_while_children_live() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let busy = Facts {
        pinned: Some("tasks 3/8".into()),
        quiet: false,
        ..facts()
    };
    assert!(
        nudges.react(COMPLETE, &busy, &log).is_none(),
        "a live descendant is the next action, so the pin reminder should wait"
    );
    assert_eq!(nudges.used, 0, "waiting must not spend nudge budget");

    let quiet = Facts {
        pinned: Some("tasks 3/8".into()),
        quiet: true,
        ..facts()
    };
    let msg = nudges
        .react(COMPLETE, &quiet, &log)
        .expect("the first quiet completion after settling should nudge");
    assert!(msg.contains("There is pinned state: tasks 3/8"));
}

/// Both obligations compose into one reminder, each on its own accounting:
/// the reply half spends budget, the pinned-state half does not.
#[test]
fn returning_agent_gets_both_reply_and_pinned_nudges() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let f = Facts {
        must_reply: true,
        pinned: Some("tasks 3/8".into()),
        ..facts()
    };
    let msg = nudges
        .react(COMPLETE, &f, &log)
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
    let log = fresh_log();
    let _ = nudges.react(EMPTY, &facts(), &log);
    assert!(nudges.used >= 1);
    nudges.reset();
    assert_eq!(nudges.used, 0, "reset clears the budget");
}

/// `Replied` answers `None` for peer and headless-root facts alike, budget
/// untouched — there is no self-verification round-trip left.
#[test]
fn any_reply_is_accepted_outright() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    for must_reply in [false, true] {
        let f = Facts {
            must_reply,
            ..facts()
        };
        assert!(nudges.react(Ok(&Outcome::Replied), &f, &log).is_none());
    }
    assert_eq!(nudges.used, 0);
}

/// The livelock regression: the same digest over repeated quiet
/// completions fires exactly once; a changed digest fires again; and
/// unpinning then re-pinning the identical digest fires again too.
#[test]
fn pin_reminder_fires_once_per_register_change() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let with = |digest: &str| Facts {
        pinned: Some(digest.into()),
        ..facts()
    };

    let first = nudges
        .react(COMPLETE, &with("tasks 3/8"), &log)
        .expect("the first sighting of a digest must nudge");
    assert!(first.contains("tasks 3/8"));
    for _ in 0..3 {
        assert!(
            nudges.react(COMPLETE, &with("tasks 3/8"), &log).is_none(),
            "a stationary digest must not re-fire"
        );
    }

    let changed = nudges
        .react(COMPLETE, &with("tasks 4/8"), &log)
        .expect("a changed digest must fire again");
    assert!(changed.contains("tasks 4/8"));

    // Unpin, then re-pin the identical digest: the edge re-arms through
    // `None`.
    let none = Facts {
        pinned: None,
        ..facts()
    };
    assert!(nudges.react(COMPLETE, &none, &log).is_none());
    let refired = nudges
        .react(COMPLETE, &with("tasks 4/8"), &log)
        .expect("re-pinning even the identical digest after an empty register must fire");
    assert!(refired.contains("tasks 4/8"));
}

/// A must-reply completion at budget with a pin due returns `None` and
/// leaves that edge armed.
#[test]
fn exhausted_reply_budget_leaves_the_pin_edge_armed() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let must_reply = Facts {
        must_reply: true,
        ..facts()
    };
    for _ in 0..BUDGET {
        assert!(nudges.react(COMPLETE, &must_reply, &log).is_some());
    }
    let pin_due = Facts {
        must_reply: true,
        pinned: Some("tasks 3/8".into()),
        ..facts()
    };
    assert!(
        nudges.react(COMPLETE, &pin_due, &log).is_none(),
        "past budget the un-replied finish is accepted"
    );

    nudges.reset();
    let no_must_reply = Facts {
        pinned: Some("tasks 3/8".into()),
        ..facts()
    };
    let msg = nudges
        .react(COMPLETE, &no_must_reply, &log)
        .expect("after reset the pin edge must still be armed");
    assert!(msg.contains("There is pinned state: tasks 3/8"), "{msg}");
}

/// `reset()` clears only the budget, not the edges; the rebirth clears
/// everything.
#[test]
fn reset_does_not_rearm_edges() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let f = Facts {
        pinned: Some("tasks 3/8".into()),
        ..facts()
    };
    assert!(nudges.react(COMPLETE, &f, &log).is_some());
    nudges.reset();
    assert!(
        nudges.react(COMPLETE, &f, &log).is_none(),
        "reset must not re-arm a told edge"
    );

    nudges = nudges.reborn();
    assert!(
        nudges.react(COMPLETE, &f, &log).is_some(),
        "the rebirth must fire again"
    );
}

/// The pressure reminder carries both the reading and the span a forced
/// eviction would take, with the line that makes the same cut by hand.
#[test]
fn pressure_names_the_cut_and_offers_the_note() {
    let body = Reminder::Pressure(Load {
        used: 400,
        total: Some(500),
        trigger: 450,
        unit: Unit::Tokens,
        planned: Some(crate::record::Span::new(1, 7).expect("a run")),
    })
    .body();
    assert!(body.contains("400 of 500 tokens"), "{body}");
    assert!(
        body.contains("Once it passes 450 tokens")
            && body.contains("that is turns 1–7. They would stay readable")
            && body.contains("evict them yourself now")
            && body.contains("`` exarch-context `evict [from: 1, to: 7, note: '…'] ``"),
        "must name the cut and offer the note: {body}"
    );
}

/// Nothing old enough to shed: the reading stands alone, with no cut to
/// announce and no note to offer.
#[test]
fn pressure_without_a_planned_cut_states_the_reading_alone() {
    let body = Reminder::Pressure(Load {
        used: 400,
        total: Some(500),
        trigger: 450,
        unit: Unit::Tokens,
        planned: None,
    })
    .body();
    assert!(body.contains("400 of 500 tokens"), "{body}");
    assert!(
        !body.contains("evict them") && body.contains("due once it passes 450 tokens"),
        "with no cut planned there is nothing to announce: {body}"
    );
}

#[test]
fn a_lifetime_budget_reminder_does_not_prescribe_an_account_top_up() {
    let body = Reminder::Ration {
        service: "openrouter".into(),
        label: "key budget · lifetime".into(),
        pct: 95,
        windowed: false,
        resumes: true,
    }
    .body();
    assert!(body.contains("limit is raised"), "{body}");
    assert!(!body.contains("topped up"), "{body}");
}

/// A reminder records its breadcrumb spending nothing: the budget is the
/// repairs' alone.
#[test]
fn a_reminder_spends_no_budget() {
    let nudges = Nudges::new(true);
    let log = fresh_log();
    let reminder = Reminder::Ration {
        service: "openrouter".into(),
        label: "5 hours".into(),
        pct: 91,
        windowed: true,
        resumes: true,
    };
    let text = nudges
        .remind(&reminder, &log)
        .expect("a steering trunk is reminded");
    assert!(
        text.contains("91% spent") && text.contains("pauses until it resets"),
        "{text}"
    );
    assert_eq!(nudges.used, 0);
}

#[test]
fn cancellation_never_nudges_or_spends_budget() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let facts = Facts {
        must_reply: true,
        pinned: Some("unfinished".into()),
        ..facts()
    };
    assert!(
        nudges
            .react(Ok(&Outcome::Cancelled), &facts, &log)
            .is_none()
    );
    assert_eq!(nudges.used, 0);
    assert!(nudges.react(COMPLETE, &facts, &log).is_some());
}

#[test]
fn pending_helpers_suppress_repairs_without_spending_or_consuming_pin_edges() {
    let mut nudges = Nudges::new(true);
    let log = fresh_log();
    let mut facts = Facts {
        must_reply: true,
        pinned: Some("unfinished".into()),
        quiet: false,
    };
    for outcome in [
        Outcome::Empty,
        Outcome::Stopped {
            reason: "early".into(),
        },
        Outcome::Complete,
    ] {
        assert!(nudges.react(Ok(&outcome), &facts, &log).is_none());
    }
    assert_eq!(nudges.used, 0);
    facts.quiet = true;
    let nudge = nudges.react(COMPLETE, &facts, &log).unwrap();
    assert!(
        nudge.contains("unfinished") && nudge.contains("`reply`"),
        "{nudge}"
    );
    assert_eq!(nudges.used, 1);
}

#[test]
fn a_self_nudge_continues_one_prompt_inside_one_job() {
    use crate::agent::testkit::{host, ral_call, scripted, set_provider, trunk};
    use crate::provider::scripted::{Reply, Script};
    use crate::record::{ChildStatus, Record, Transcript, Voice};
    use std::time::{Duration, Instant};

    let mut root = trunk(true);
    set_provider(
        &mut root,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::empty())
                .then(Reply::tool_calls(vec![ral_call(
                    "answer",
                    "exarch-agents `reply 'done'",
                )])),
        ),
    );
    root.handle().steer("one prompt".into()).unwrap();
    let host = host(root);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut records = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let signal = host
            .feed()
            .recv_timeout(remaining)
            .expect("the nudged job settles within five seconds");
        if let crate::bus::Signal::Commit(_, commit) = signal {
            records.extend(
                commit
                    .records()
                    .iter()
                    .map(|recorded| recorded.value().clone()),
            );
            if records.iter().any(|r| {
                matches!(
                    r,
                    Record::Control(crate::record::Control::JobSettled { .. })
                )
            }) {
                break;
            }
        }
        assert!(Instant::now() < deadline, "the nudged job must settle");
    }
    assert_eq!(host.end(), Vec::<String>::new());
    let opening = records
        .iter()
        .position(|r| matches!(r, Record::Control(crate::record::Control::JobOpened)))
        .unwrap();
    let prompt = records
        .iter()
        .position(|r| matches!(r, Record::Transcript(Transcript::UserPrompt { .. })))
        .unwrap();
    assert!(
        opening < prompt,
        "the job is recorded before its first exchange record"
    );
    let count = |predicate: fn(&Record) -> bool| records.iter().filter(|r| predicate(r)).count();
    assert_eq!(
        count(|r| matches!(r, Record::Control(crate::record::Control::JobOpened))),
        1
    );
    assert_eq!(
        count(|r| matches!(
            r,
            Record::Control(crate::record::Control::JobSettled { .. })
        )),
        1
    );
    assert_eq!(
        count(|r| matches!(r, Record::Forensic(Forensic::TurnStarted { .. }))),
        2
    );
    assert_eq!(
        count(|r| matches!(
            r,
            Record::Transcript(Transcript::UserPrompt {
                voice: Voice::Human,
                ..
            })
        )),
        1
    );
    assert_eq!(
        count(|r| matches!(
            r,
            Record::Transcript(Transcript::UserPrompt {
                voice: Voice::Harness,
                ..
            })
        )),
        0
    );
    assert_eq!(
        count(
            |r| matches!(r, Record::Transcript(Transcript::Steering { text, voice: Voice::Harness }) if text.contains("no text and no tool calls"))
        ),
        1
    );
    assert_eq!(
        count(|r| matches!(
            r,
            Record::Control(crate::record::Control::Status {
                status: ChildStatus::Replied(_)
            })
        )),
        1
    );
}
