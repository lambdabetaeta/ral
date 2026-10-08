use super::{Inbox, Item, Minted, Next, ParkMode, Post, Source, Stamped};
use crate::bus::{AgentMessage, AgentOutcome, AgentResult, ScheduleId};
use crate::bus::{Read, Rewrite};
use crate::cancel;
use crate::record::AgentId;
use ral_core::test_helper::eventually;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The deliveries of one mid-exchange drain, for tests that queue no reads.
fn drained(inbox: &Inbox) -> Vec<Item> {
    inbox
        .drain_mid_exchange()
        .into_iter()
        .map(|next| match next {
            Next::Item(item) => item,
            other => panic!("expected a delivery, got {other:?}"),
        })
        .collect()
}

/// Epoch 0 is a fresh [`Inbox`]'s own; `id` matters only to the dedupe tests.
fn wakeup(id: u64, label: &str, trigger: &str, prompt: &str) -> Post {
    wakeup_at(id, label, trigger, prompt, 0)
}

/// [`wakeup`] with an explicit epoch, for the stale-admission tests.
/// Forging a [`Minted`] is possible only here inside `bus`; everyone
/// else sends through a [`Stamp`].
fn wakeup_at(id: u64, label: &str, trigger: &str, prompt: &str, epoch: u64) -> Post {
    Post::Stamped {
        epoch: Minted(epoch),
        kind: Stamped::Wakeup {
            id: ScheduleId::new(id),
            label: label.into(),
            trigger: trigger.into(),
            prompt: prompt.into(),
        },
    }
}

#[test]
fn inbox_waiting_for_input_tracks_human_park() {
    let inbox = Inbox::new();
    assert!(
        inbox.waiting_for_input(),
        "a fresh interactive inbox starts at the human boundary"
    );

    inbox.push_user("work".into());
    assert!(
        !inbox.waiting_for_input(),
        "posting input wakes the consumer out of the yielded state"
    );
    assert!(matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "work"));
    assert!(
        !inbox.waiting_for_input(),
        "draining an item means work has started; yield resumes only at park"
    );

    let worker_inbox = inbox.clone();
    let token = cancel::Token::new();
    let worker_token = token;
    let handle =
        std::thread::spawn(move || worker_inbox.next_or_idle(|_| ParkMode::Held, &worker_token));

    assert!(
        eventually(Duration::from_secs(1), || inbox
            .waiting_for_input()
            .then_some(()))
        .is_some(),
        "a Held empty-inbox park is the human-input yield point"
    );

    inbox.mailbox().push_user("next".into());
    assert!(
        !inbox.waiting_for_input(),
        "a submitted prompt clears the yielded bit before waking the worker"
    );
    assert!(
        matches!(handle.join().expect("parked worker joins"), Some(Next::Item(Item::Human(s))) if s == "next"),
        "the wakeup delivered the submitted prompt"
    );
    assert!(
        !inbox.waiting_for_input(),
        "taking the item leaves the root working until it parks again"
    );
}

#[test]
fn inbox_waiting_for_input_ignores_non_human_parks() {
    let inbox = Inbox::new();
    inbox.push_user("work".into());
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Human(_)))
    ));
    assert!(!inbox.waiting_for_input());

    let observed = Arc::new(AtomicBool::new(false));
    let worker_observed = observed.clone();
    let worker_inbox = inbox.clone();
    let token = cancel::Token::new();
    let worker_token = token.clone();
    let handle = std::thread::spawn(move || {
        worker_inbox.next_or_idle(
            |_| {
                worker_observed.store(true, Ordering::Release);
                ParkMode::HeldByChildren
            },
            &worker_token,
        )
    });

    assert!(
        eventually(Duration::from_secs(1), || observed
            .load(Ordering::Acquire)
            .then_some(()))
        .is_some(),
        "the worker reached the park predicate"
    );
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !inbox.waiting_for_input(),
        "waiting on children is still work, not a human-input yield"
    );

    token.cancel(ral_core::process::CancelCause::Cancelled);
    assert!(
        handle.join().expect("cancelled worker joins").is_none(),
        "non-human parks terminate on cancellation"
    );
}

/// The complement of the test above: an *interrupt* drops the in-flight
/// exchange rather than ending the agent.  Proved without a timing race —
/// after the interrupt the only exit left is a pushed item, so getting it
/// back through the join is the evidence a terminate would have destroyed.
#[test]
fn non_human_park_survives_an_interrupt() {
    let inbox = Inbox::new();
    inbox.push_user("work".into());
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Human(_)))
    ));

    let observed = Arc::new(AtomicBool::new(false));
    let worker_observed = observed.clone();
    let worker_inbox = inbox.clone();
    let token = cancel::Token::new();
    let worker_token = token.clone();
    let handle = std::thread::spawn(move || {
        worker_inbox.next_or_idle(
            |_| {
                worker_observed.store(true, Ordering::Release);
                ParkMode::HeldByChildren
            },
            &worker_token,
        )
    });

    assert!(
        eventually(Duration::from_secs(1), || observed
            .load(Ordering::Acquire)
            .then_some(()))
        .is_some(),
        "the worker reached the park predicate"
    );

    token.cancel(ral_core::process::CancelCause::Interrupted);

    inbox.mailbox().push_user("resume".into());
    assert!(
        matches!(
            handle.join().expect("parked worker joins"),
            Some(Next::Item(Item::Human(s))) if s == "resume"
        ),
        "the interrupt was ignored; the pushed item released the park"
    );
}

/// [`ParkMode::Engaged`] grants no immunity: were it to, its
/// `HeldByChildren` parent would wait forever on a cancelled result.
#[test]
fn engaged_park_dies_on_a_terminate_cause_despite_the_exchange() {
    let inbox = Inbox::new();
    let observed = Arc::new(AtomicBool::new(false));
    let worker_observed = observed.clone();
    let token = cancel::Token::new();
    let worker_token = token.clone();
    let handle = std::thread::spawn(move || {
        inbox.next_or_idle(
            |_| {
                worker_observed.store(true, Ordering::Release);
                ParkMode::Engaged
            },
            &worker_token,
        )
    });

    assert!(
        eventually(Duration::from_secs(1), || observed
            .load(Ordering::Acquire)
            .then_some(()))
        .is_some(),
        "the worker reached the park predicate"
    );

    token.cancel(ral_core::process::CancelCause::Cancelled);
    assert!(
        handle.join().expect("cancelled worker joins").is_none(),
        "a terminate cause ends an Engaged park despite the exchange"
    );
}

/// The complement: a conversing [`ParkMode::Held`] stays immune even to a
/// terminate cause, proved as [`non_human_park_survives_an_interrupt`] is.
#[test]
fn held_park_survives_a_terminate_cause() {
    let inbox = Inbox::new();
    inbox.push_user("work".into());
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Human(_)))
    ));

    let observed = Arc::new(AtomicBool::new(false));
    let worker_observed = observed.clone();
    let worker_inbox = inbox.clone();
    let token = cancel::Token::new();
    let worker_token = token.clone();
    let handle = std::thread::spawn(move || {
        worker_inbox.next_or_idle(
            |_| {
                worker_observed.store(true, Ordering::Release);
                ParkMode::Held
            },
            &worker_token,
        )
    });

    assert!(
        eventually(Duration::from_secs(1), || observed
            .load(Ordering::Acquire)
            .then_some(()))
        .is_some(),
        "the worker reached the park predicate"
    );

    token.cancel(ral_core::process::CancelCause::Cancelled);

    inbox.mailbox().push_user("resume".into());
    assert!(
        matches!(
            handle.join().expect("parked worker joins"),
            Some(Next::Item(Item::Human(s))) if s == "resume"
        ),
        "a live conversation ignores even a terminate cause"
    );
}

/// A wakeup reaches the model as soon as the tool batch settles.
#[test]
fn inbox_wakeup_drains_at_tool_boundary_marked() {
    let inbox = Inbox::new();
    inbox.push_user("steer".into());
    inbox.push(wakeup(1, "nightly", "0 3 * * *", "run the tests"));

    assert!(
        matches!(
            drained(&inbox).as_slice(),
            [Item::Human(h), Item::Wakeup(w)]
                if h == "steer"
                    && w == "[scheduled 'nightly' · 0 3 * * *] run the tests",
        ),
        "the wakeup drains mid-exchange, marked, after the steering",
    );
    assert!(inbox.is_empty());
}

/// Async deliveries drain in queue order too, so a result settling during a
/// long tool-call loop need not wait for the exchange to end.
#[test]
fn inbox_tool_drain_takes_async_deliveries() {
    let inbox = Inbox::new();
    inbox.push(wakeup(1, "nightly", "@", "go"));
    inbox.push_user("redirect now".into());
    inbox.push_user("and also this".into());

    assert!(
        matches!(
            drained(&inbox).as_slice(),
            [Item::Wakeup(_), Item::Human(s)] if s == "redirect now\nand also this",
        ),
        "the async wakeup and the coalesced steering both drain, in order",
    );
    assert!(inbox.is_empty());
}

#[test]
fn inbox_agent_message_drains_marked_at_tool_boundary() {
    let inbox = Inbox::new();
    inbox.push(Post::AgentMessage(AgentMessage {
        from: AgentId::new(7),
        from_name: "review".into(),
        text: "please inspect the parser branch".into(),
    }));

    assert!(matches!(
        drained(&inbox).as_slice(),
        [Item::Message(m)]
            if m.from == AgentId::new(7)
                && m.from_name == "review"
                && m.text == "please inspect the parser branch"
                && m.render()
                    == "[EXARCH AGENT 7 MESSAGE: review]\nplease inspect the parser branch\n[/EXARCH]"
    ));
    assert!(inbox.is_empty());
}

/// A `/rewind` is a barrier: it drops the very context a prompt typed after
/// it would land in, so nothing queued behind it drains until it has run.
/// Deliveries ahead of it still drain.
#[test]
fn inbox_tool_drain_stops_at_a_barrier() {
    let inbox = Inbox::new();
    inbox.push_user("before".into());
    inbox.push(wakeup(1, "x", "@", "p"));
    inbox.push(Post::Rewrite(Rewrite::Rewind(7)));
    inbox.push_user("after the rewind".into());

    assert!(matches!(
        drained(&inbox).as_slice(),
        [Item::Human(b), Item::Wakeup(_)] if b == "before"
    ));
    assert!(drained(&inbox).is_empty());
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Rewrite(Rewrite::Rewind(7)))
    ));
    assert!(
        matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "after the rewind")
    );
    assert!(inbox.is_empty());
}

/// A read drains mid-exchange in the order typed, beside the prompts; a
/// rewrite holds the scan, and everything behind it waits for the exchange
/// boundary.
#[test]
fn inbox_mid_exchange_drain_takes_reads_in_order_and_holds_at_a_rewrite() {
    let inbox = Inbox::new();
    inbox.push(Post::Read(Read::Resources));
    inbox.push_user("a".into());
    inbox.push(Post::Rewrite(Rewrite::Evict));
    inbox.push_user("b".into());

    assert!(matches!(
        inbox.drain_mid_exchange().as_slice(),
        [Next::Read(Read::Resources), Next::Item(Item::Human(s))] if s == "a"
    ));
    assert!(inbox.drain_mid_exchange().is_empty(), "the rewrite holds");
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Rewrite(Rewrite::Evict))
    ));
    assert!(matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "b"));
    assert!(inbox.is_empty());
}

/// The queue strip is a projection of what the human typed, not an inbox
/// debugger: prompts and commands alike, in the order they were typed, and
/// a wakeup — which nobody typed — stays out.
#[test]
fn inbox_queued_human_messages_shows_typed_text_in_order() {
    let inbox = Inbox::new();
    inbox.push(wakeup(1, "morning", "@daily", "check"));
    inbox.push_user("first".into());
    inbox.push(Post::Read(Read::Branch(Some("scout".into()))));
    inbox.push_user("second".into());

    assert_eq!(
        inbox.queued_human_messages(),
        vec![
            "first".to_string(),
            "/branch scout".to_string(),
            "second".to_string()
        ]
    );
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Wakeup(_)))
    ));
    assert!(matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "first"));
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Read(Read::Branch(Some(s)))) if s == "scout"
    ));
    assert!(matches!(inbox.next_item(), Some(Next::Item(Item::Human(s))) if s == "second"));
}

/// A sole wakeup is not the user's draft, so nothing comes back.
#[test]
fn inbox_pop_back_user_all_no_user_prompts() {
    let inbox = Inbox::new();
    inbox.push(wakeup(1, "x", "@", "p"));
    assert_eq!(inbox.pop_back_user_all(), None, "no user prompts to recall");
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Wakeup(_)))
    ));
}

/// Even prompts sandwiched between non-user deliveries, which keep their
/// order.  "second" and "third" arrive back-to-back, so the push-time merge
/// already folded them into one entry.
#[test]
fn inbox_pop_back_user_all_extracts_all_leaving_non_user_in_order() {
    let inbox = Inbox::new();
    inbox.push_user("first".into());
    inbox.push(wakeup(1, "x", "@", "p"));
    inbox.push_user("second".into());
    inbox.push_user("third".into());
    inbox.push(Post::Read(Read::Resources));
    inbox.push_user("fourth".into());
    assert_eq!(
        inbox.pop_back_user_all(),
        Some(vec![
            "first".to_string(),
            "second\nthird".to_string(),
            "fourth".to_string(),
        ]),
        "all user prompts come back oldest-first, past interspersed deliveries",
    );
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Wakeup(_)))
    ));
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Read(Read::Resources))
    ));
    assert!(inbox.is_empty());
}

/// A `spawn` worker's batch, terminated by the `` `done `` event core
/// appends, stamped with `epoch` — a live one for the ordinary path, a
/// stale one to exercise the pop-time fence.
fn surface(epoch: u64) -> Post {
    use crate::card::testkit::{map_value, s, variant};
    let done = variant(
        "done",
        map_value(vec![
            ("cmd", s("<block>")),
            (
                "outcome",
                variant("ok", ral_core::first_order::FOValue::Unit),
            ),
        ]),
    );
    Post::Stamped {
        epoch: Minted(epoch),
        kind: Stamped::Surface {
            id: AgentId::new(0),
            values: vec![done],
        },
    }
}

/// A `/clear` landing between delivery and drain empties the deque.
#[test]
fn inbox_surface_drains_at_tool_boundary_and_cleared() {
    let inbox = Inbox::new();
    inbox.push(surface(inbox.mailbox().epoch()));
    inbox.clear();
    assert!(
        drained(&inbox).is_empty(),
        "a /clear drops the queued batch"
    );

    inbox.push(surface(inbox.mailbox().epoch()));
    assert!(matches!(
        drained(&inbox).as_slice(),
        [Item::Surface { id, .. }] if *id == AgentId::new(0)
    ));
}

/// Draining a wakeup re-opens its schedule for the next occurrence: the
/// queue itself is the overlap-check's source of truth, so popping the
/// item is all `has_queued_wakeup` needs to flip.
#[test]
fn inbox_wakeup_leaves_the_queue_once_drained() {
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    mailbox.stamp().post(Stamped::Wakeup {
        id: ScheduleId::new(1),
        label: "n".into(),
        trigger: "* * * * *".into(),
        prompt: "go".into(),
    });
    assert!(mailbox.has_queued_wakeup(ScheduleId::new(1)));
    let _ = inbox.next_item();
    assert!(
        !mailbox.has_queued_wakeup(ScheduleId::new(1)),
        "draining the wakeup re-opens its schedule"
    );
}

/// `clear` drops the queue outright, so an unconsumed wakeup is not
/// stranded as "still queued" forever.  `Avatar::clear` disarms the
/// schedule registry alongside, but the TUI's `App::clear` reaches only
/// the inbox, so this must hold on its own.
#[test]
fn inbox_clear_leaves_no_wakeup_stranded_as_queued() {
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    mailbox.stamp().post(Stamped::Wakeup {
        id: ScheduleId::new(1),
        label: "n".into(),
        trigger: "* * * * *".into(),
        prompt: "go".into(),
    });
    inbox.clear();
    assert!(
        !mailbox.has_queued_wakeup(ScheduleId::new(1)),
        "clear must not strand a wakeup as still queued"
    );
    assert!(inbox.is_empty());
}

/// The reaper's compose-then-push race (`ScheduleRegistry::fire`): a wakeup
/// composed under an epoch an intervening `/clear` has since bumped never
/// surfaces into the rebuilt context.
#[test]
fn stale_epoch_wakeup_is_refused_at_pop() {
    let inbox = Inbox::new();
    let stale = inbox.mailbox().epoch();
    inbox.clear();
    inbox.push(wakeup_at(1, "n", "@", "go", stale));
    assert!(
        inbox.next_item().is_none(),
        "a wakeup stamped with an epoch older than the inbox's own is dropped"
    );
}

/// The positive half: the live epoch is delivered like any other message.
#[test]
fn current_epoch_wakeup_is_delivered() {
    let inbox = Inbox::new();
    let live = inbox.mailbox().epoch();
    inbox.push(wakeup_at(1, "n", "@", "go", live));
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Wakeup(_)))
    ));
}

/// The same one-rule fence, over an `AgentResult` instead of a wakeup.
#[test]
fn stale_epoch_agent_result_is_refused_at_pop() {
    let inbox = Inbox::new();
    let stamp = inbox.mailbox().stamp();
    inbox.clear();
    stamp.post(Stamped::AgentResult(AgentResult {
        id: AgentId::new(1),
        name: "worker".into(),
        outcome: AgentOutcome::Stopped("done".into()),
        elapsed: Duration::ZERO,
    }));
    assert!(
        inbox.next_item().is_none(),
        "an agent result stamped with an epoch older than the inbox's own is dropped"
    );
}

/// The positive half: a live-epoch `AgentResult` is delivered.
#[test]
fn current_epoch_agent_result_is_delivered() {
    let inbox = Inbox::new();
    inbox
        .mailbox()
        .stamp()
        .post(Stamped::AgentResult(AgentResult {
            id: AgentId::new(1),
            name: "worker".into(),
            outcome: AgentOutcome::Stopped("done".into()),
            elapsed: Duration::ZERO,
        }));
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Agent(_)))
    ));
}

/// The same one-rule fence, over a `Surface` batch instead of a wakeup.
#[test]
fn stale_epoch_surface_batch_is_refused_at_pop() {
    let inbox = Inbox::new();
    let stale = inbox.mailbox().epoch();
    inbox.clear();
    inbox.push(surface(stale));
    assert!(
        inbox.next_item().is_none(),
        "a surface batch stamped with an epoch older than the inbox's own is dropped"
    );
}

/// The positive half: a live-epoch `Surface` batch is delivered.
#[test]
fn current_epoch_surface_batch_is_delivered() {
    let inbox = Inbox::new();
    let live = inbox.mailbox().epoch();
    inbox.push(surface(live));
    assert!(matches!(
        inbox.next_item(),
        Some(Next::Item(Item::Surface { .. }))
    ));
}

// ── inbox quotas without silent loss ───────────────────────────────────

fn depth_of(inbox: &Inbox, source: Source) -> u64 {
    inbox
        .source_depths()
        .into_iter()
        .find(|(s, _)| *s == source)
        .map_or(0, |(_, n)| n)
}

/// One entry, not two, and a different schedule's is untouched and keeps
/// its arrival order.
#[test]
fn inbox_scheduled_wakeup_dedupes_by_schedule_id_newest_wins() {
    let inbox = Inbox::new();
    inbox.push(wakeup(1, "nightly", "@daily", "first"));
    inbox.push(wakeup(1, "nightly", "@daily", "second"));
    inbox.push(wakeup(2, "morning", "@daily", "other schedule"));
    assert_eq!(
        depth_of(&inbox, Source::Schedule),
        2,
        "schedule 1 replaced in place; schedule 2 is its own entry"
    );
    match inbox.next_item() {
        Some(Next::Item(Item::Wakeup(text))) => assert!(
            text.contains("second") && !text.contains("first"),
            "the newest wakeup for schedule 1 wins: {text}"
        ),
        _ => panic!("expected schedule 1's (replaced) wakeup first"),
    }
    match inbox.next_item() {
        Some(Next::Item(Item::Wakeup(text))) => assert!(text.contains("other schedule")),
        _ => panic!("expected schedule 2's wakeup, arrival order preserved"),
    }
}

/// Otherwise a fast typist grows the queue one entry per line.
#[test]
fn inbox_user_steering_merges_pre_boundary_preserving_order() {
    let inbox = Inbox::new();
    inbox.push_user("first line".into());
    inbox.push_user("second line".into());
    assert_eq!(
        depth_of(&inbox, Source::User),
        1,
        "consecutive steering merges into one entry at push time"
    );
    match inbox.next_item() {
        Some(Next::Item(Item::Human(text))) => {
            assert_eq!(
                text, "first line\nsecond line",
                "both texts survive in order"
            );
        }
        _ => panic!("expected a merged Human item"),
    }
}

/// The agent self-pushes a nudge per deliberation, so a second one means a
/// fresher continuation superseded the first, not that both are owed.
#[test]
fn inbox_nudge_replaces_a_still_queued_one_newest_wins() {
    let inbox = Inbox::new();
    inbox.push(Post::Nudge {
        prompt: 1,
        text: "retry".into(),
    });
    inbox.push(Post::Nudge {
        prompt: 1,
        text: "retry".into(),
    });
    inbox.push(Post::Nudge {
        prompt: 2,
        text: "different".into(),
    });
    assert_eq!(
        depth_of(&inbox, Source::Nudge),
        1,
        "a nudge never grows past one outstanding entry"
    );
    assert!(
        matches!(inbox.next_item(), Some(Next::Item(Item::Nudge { text, .. })) if text == "different"),
        "the newest nudge is the one delivered"
    );
}

/// Dropping the nudge between two steering lines leaves them adjacent, and
/// the inbox restores the push-time merge rather than deliver them apart.
#[test]
fn inbox_drop_nudges_remerges_the_steering_it_leaves_adjacent() {
    let inbox = Inbox::new();
    inbox.push_user("first".into());
    inbox.push(Post::Nudge {
        prompt: 1,
        text: "retry".into(),
    });
    inbox.push_user("second".into());
    inbox.drop_nudges();
    assert_eq!(depth_of(&inbox, Source::User), 1);
    assert!(
        matches!(inbox.next_item(), Some(Next::Item(Item::Human(text))) if text == "first\nsecond")
    );
    assert!(inbox.is_empty());
}
