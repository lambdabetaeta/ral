use super::*;
use crate::agent::testkit::{open_next, pop, pop_outcome};
use crate::record::Memory;
use ral_core::process::DurableRoot;
use ral_core::test_helper::eventually;
use std::sync::atomic::{AtomicBool, Ordering};

fn inbox() -> Inbox {
    Inbox::new(DurableRoot::default().child(), None)
}

fn draw(inbox: &Inbox) -> Option<Drawn> {
    inbox.draw(|_| ParkMode::Quiesce)
}

/// The ticket a stopped job hands back, still holding the job open.
fn stopped(job: Job) -> StoppedJob {
    match job.close() {
        Closed::Stopped(held) => held,
        Closed::Ran => panic!("the job was stopped, so it cannot close as running"),
    }
}

fn job_is_open(inbox: &Inbox) -> bool {
    inbox.0.lock().job.is_some()
}

/// An active inbox and the job its "running" prompt opened.
fn running() -> (Inbox, Job) {
    let inbox = active();
    inbox.mailbox().steer("running".into());
    let job = inbox.await_job().unwrap();
    (inbox, job)
}

fn active() -> Inbox {
    let inbox = inbox();
    assert!(inbox.mailbox().steer("start".into()));
    assert!(matches!(open_next(&inbox), Next::Item(Item::Human(_))));
    inbox
}

fn drained(inbox: &Inbox) -> Vec<Item> {
    inbox
        .drain()
        .into_iter()
        .map(|next| match next {
            Next::Item(item) => item,
            other => panic!("expected a delivery, got {other:?}"),
        })
        .collect()
}

fn wakeup(id: u64, label: &str, trigger: &str, prompt: &str) -> Scoped {
    Scoped::Wakeup {
        id: ScheduleId::new(id),
        label: label.into(),
        trigger: trigger.into(),
        prompt: prompt.into(),
    }
}

fn result(outcome: AgentOutcome) -> Scoped {
    Scoped::Result {
        result: AgentResult {
            name: "worker".into(),
            outcome,
            elapsed: Duration::ZERO,
        },
        paid: Vec::new(),
    }
}

fn follow_up(text: &str) -> AgentMessage {
    AgentMessage {
        from: AgentId::new(7),
        from_name: "review".into(),
        text: text.into(),
    }
}

fn message() -> Scoped {
    Scoped::Message {
        message: follow_up("please inspect the parser branch"),
        owed: None,
    }
}

fn surface() -> Scoped {
    use crate::card::testkit::{map_value, s, variant};
    Scoped::Surface {
        id: AgentId::new(0),
        values: vec![variant(
            "done",
            map_value(vec![
                ("cmd", s("<block>")),
                ("outcome", variant("ok", FOValue::Unit)),
            ]),
        )],
    }
}

#[test]
fn abandoning_a_clear_boundary_changes_nothing() {
    let inbox = active();
    let mailbox = inbox.mailbox();
    mailbox.steer("old".into());
    mailbox.post(wakeup(1, "old", "@", "go"));
    let boundary = inbox.clear_boundary();
    mailbox.steer("new".into());
    let _ = boundary;
    assert!(mailbox.is_current());
    assert_eq!(mailbox.queued(), ["old", "new"]);
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "old"));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Wakeup(_)))));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "new"));
}

#[test]
fn clear_splits_a_coalesced_human_tail_at_its_captured_boundary() {
    for observe in [false, true] {
        let inbox = active();
        let mailbox = inbox.mailbox();
        mailbox.steer("old α".into());
        mailbox.steer("old β".into());
        let boundary = inbox.clear_boundary();
        mailbox.steer(String::new());
        mailbox.steer("new γ".into());
        if observe {
            assert_eq!(mailbox.queued(), ["old α\nold β\n\nnew γ"]);
            assert_eq!(depth_of(&inbox, Source::User), 1);
        }
        inbox.apply_clear(boundary);
        assert_eq!(mailbox.queued(), ["\nnew γ"]);
        assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "\nnew γ"));
        assert!(inbox.is_empty());
    }
}

#[test]
fn clear_boundary_survives_provenance_fencing_and_a_racing_recall() {
    for recall in [false, true] {
        let inbox = active();
        let mailbox = inbox.mailbox();
        mailbox.steer("old".into());
        inbox.nudge(1, "old exchange".into());
        let boundary = inbox.clear_boundary();
        mailbox.steer("after boundary".into());
        inbox.close_exchange();
        assert_eq!(mailbox.queued(), ["old\nafter boundary"]);
        if recall {
            assert_eq!(mailbox.recall(), ["old\nafter boundary"]);
            mailbox.steer("after recall".into());
        }
        inbox.apply_clear(boundary);
        assert_eq!(
            mailbox.queued(),
            [if recall {
                "after recall"
            } else {
                "after boundary"
            }]
        );
    }
}

#[test]
fn committed_clear_preserves_later_commands_but_refuses_old_scoped_producers() {
    let inbox = active();
    let old = inbox.mailbox();
    old.steer("old".into());
    old.post_read(Read::Resources);
    let boundary = inbox.clear_boundary();
    old.steer("new".into());
    old.post_read(Read::Resources);
    old.post_rewrite(Rewrite::Evict);
    for kind in [
        surface(),
        message(),
        wakeup(1, "old", "@", "go"),
        result(AgentOutcome::Replied),
    ] {
        assert!(old.post(kind));
    }
    inbox.apply_clear(boundary);
    for kind in [
        surface(),
        message(),
        wakeup(1, "late", "@", "go"),
        result(AgentOutcome::Replied),
    ] {
        assert!(!old.post(kind));
    }
    assert!(!old.is_current());
    assert!(inbox.mailbox().is_current());
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "new"));
    assert!(matches!(pop(&inbox), Some(Next::Read(Read::Resources))));
    assert!(matches!(pop(&inbox), Some(Next::Rewrite(Rewrite::Evict))));
    assert!(inbox.is_empty());
}

#[test]
fn every_scoped_kind_is_fenced_at_pop() {
    let cases: [(Source, fn() -> Scoped); 5] = [
        (Source::Agent, || {
            result(AgentOutcome::Stopped("done".into()))
        }),
        (Source::Surface, surface),
        (Source::Schedule, || wakeup(1, "nightly", "@", "go")),
        (Source::Message, message),
        (Source::Nudge, || Scoped::Nudge {
            prompt: 1,
            text: "continue".into(),
        }),
    ];
    for (source, kind) in cases {
        for cancelled in [false, true] {
            let inbox = inbox();
            let from = DurableRoot::default().child();
            let mailbox = inbox.mailbox().vouched_by((*from).clone());
            mailbox.post(kind());
            if cancelled {
                from.cancel(CancelCause::Cancelled);
            }
            assert_eq!(
                depth_of(&inbox, source),
                u64::from(!cancelled),
                "observers fence: {source:?}"
            );
            assert_eq!(pop(&inbox).is_some(), !cancelled, "{source:?}");
            assert!(inbox.is_empty());
            mailbox.steer("human".into());
            assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "human"));
        }
    }
}

#[test]
fn dead_nudge_remerges_the_human_posts_it_leaves_adjacent() {
    for recall in [false, true] {
        let inbox = active();
        let mailbox = inbox.life_mailbox();
        mailbox.steer("first".into());
        inbox.nudge(1, "continue".into());
        mailbox.steer("second".into());
        inbox.close_exchange();
        if recall {
            assert_eq!(mailbox.recall(), ["first\nsecond"]);
        } else {
            assert!(
                matches!(pop(&inbox), Some(Next::Item(Item::Human(text))) if text == "first\nsecond")
            );
        }
        assert!(inbox.is_empty());
    }
}

#[test]
fn dead_provenance_or_life_refuses_publication() {
    let inbox = inbox();
    let from = inbox.life().child();
    let mailbox = inbox.life_mailbox().vouched_by((*from).clone());
    from.cancel(CancelCause::Cancelled);
    assert!(!mailbox.post(surface()));
    inbox.life_mailbox().end(CancelCause::Cancelled);
    assert!(!inbox.life_mailbox().post(message()));
    assert!(inbox.is_empty());
}

#[test]
fn commands_are_not_fenced_by_abandoned_work() {
    let inbox = inbox();
    let mailbox = inbox.mailbox();
    mailbox.post_read(Read::Resources);
    mailbox.post_rewrite(Rewrite::Evict);
    inbox.abandon(Queue::Keep);
    assert!(matches!(pop(&inbox), Some(Next::Read(Read::Resources))));
    assert!(matches!(pop(&inbox), Some(Next::Rewrite(Rewrite::Evict))));
}

#[test]
fn inbox_waiting_for_input_tracks_human_park() {
    let inbox = inbox();
    let mailbox = inbox.mailbox();
    assert!(mailbox.status().waiting_for_input);
    mailbox.steer("work".into());
    assert!(!mailbox.status().waiting_for_input);
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "work"));
    assert!(!mailbox.status().waiting_for_input);
    let worker = std::thread::spawn(move || inbox.draw(|_| ParkMode::Held).map(|drawn| drawn.next));
    assert!(
        eventually(Duration::from_secs(1), || mailbox
            .status()
            .waiting_for_input
            .then_some(()))
        .is_some()
    );
    mailbox.steer("next".into());
    assert!(!mailbox.status().waiting_for_input);
    assert!(matches!(worker.join().unwrap(), Some(Next::Item(Item::Human(s))) if s == "next"));
    assert!(!mailbox.status().waiting_for_input);
}

#[test]
fn inbox_waiting_for_input_ignores_non_human_parks() {
    let inbox = active();
    let mailbox = inbox.mailbox();
    let observed = Arc::new(AtomicBool::new(false));
    let worker_observed = observed.clone();
    let worker = std::thread::spawn(move || {
        inbox.draw(|_| {
            worker_observed.store(true, Ordering::Release);
            ParkMode::HeldByChildren
        })
    });
    assert!(
        eventually(Duration::from_secs(1), || observed
            .load(Ordering::Acquire)
            .then_some(()))
        .is_some()
    );
    assert!(!mailbox.status().waiting_for_input);
    mailbox.end(CancelCause::Cancelled);
    assert!(worker.join().unwrap().is_none());
}

#[test]
fn non_human_park_survives_an_interrupt() {
    let inbox = active();
    let mailbox = inbox.mailbox();
    let observed = AtomicBool::new(false);
    let next = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            inbox
                .draw(|_| {
                    observed.store(true, Ordering::Release);
                    ParkMode::HeldByChildren
                })
                .map(|drawn| drawn.next)
        });
        assert!(
            eventually(Duration::from_secs(1), || observed
                .load(Ordering::Acquire)
                .then_some(()))
            .is_some()
        );
        mailbox.interrupt();
        mailbox.steer("resume".into());
        worker.join().unwrap()
    });
    assert!(matches!(next, Some(Next::Item(Item::Human(s))) if s == "resume"));
    assert!(mailbox.is_live());
}

#[test]
fn engaged_park_ends_with_its_life() {
    let inbox = inbox();
    let mailbox = inbox.mailbox();
    let observed = Arc::new(AtomicBool::new(false));
    let worker_observed = observed.clone();
    let worker = std::thread::spawn(move || {
        inbox.draw(|_| {
            worker_observed.store(true, Ordering::Release);
            ParkMode::Engaged
        })
    });
    assert!(
        eventually(Duration::from_secs(1), || observed
            .load(Ordering::Acquire)
            .then_some(()))
        .is_some()
    );
    mailbox.end(CancelCause::Cancelled);
    assert!(worker.join().unwrap().is_none());
}

#[test]
fn parked_owner_polls_ancestor_cancellation() {
    let root = DurableRoot::default();
    let inbox = Inbox::new(root.child(), None);
    let observed = Arc::new(AtomicBool::new(false));
    let worker_observed = observed.clone();
    let worker = std::thread::spawn(move || {
        inbox.draw(|_| {
            worker_observed.store(true, Ordering::Release);
            ParkMode::UntilCancelled
        })
    });
    assert!(
        eventually(Duration::from_secs(1), || observed
            .load(Ordering::Acquire)
            .then_some(()))
        .is_some()
    );
    root.cancel(CancelCause::Cancelled);
    assert!(
        eventually(Duration::from_secs(1), || worker
            .is_finished()
            .then_some(()))
        .is_some()
    );
    assert!(worker.join().unwrap().is_none());
}

#[test]
fn inbox_wakeup_drains_at_tool_boundary_marked() {
    let inbox = active();
    inbox.mailbox().steer("steer".into());
    inbox
        .mailbox()
        .post(wakeup(1, "nightly", "0 3 * * *", "run the tests"));
    assert!(
        matches!(drained(&inbox).as_slice(), [Item::Human(h), Item::Wakeup(w)]
        if h == "steer" && w == "[scheduled 'nightly' · 0 3 * * *] run the tests")
    );
    assert!(inbox.is_empty());
}

#[test]
fn inbox_tool_drain_takes_async_deliveries() {
    let inbox = active();
    inbox.mailbox().post(wakeup(1, "nightly", "@", "go"));
    inbox.mailbox().steer("redirect now".into());
    inbox.mailbox().steer("and also this".into());
    assert!(
        matches!(drained(&inbox).as_slice(), [Item::Wakeup(_), Item::Human(s)]
        if s == "redirect now\nand also this")
    );
    assert!(inbox.is_empty());
}

#[test]
fn inbox_agent_message_drains_marked_at_tool_boundary() {
    let inbox = active();
    inbox.mailbox().post(message());
    assert!(matches!(drained(&inbox).as_slice(), [Item::Message(m)]
        if m.from == AgentId::new(7) && m.from_name == "review"
        && m.text == "please inspect the parser branch"
        && m.render() == "[EXARCH AGENT 7 MESSAGE: review]\nplease inspect the parser branch\n[/EXARCH]"));
    assert!(inbox.is_empty());
}

#[test]
fn inbox_tool_drain_stops_at_a_barrier() {
    let inbox = active();
    let mailbox = inbox.mailbox();
    mailbox.steer("before".into());
    mailbox.post(wakeup(1, "x", "@", "p"));
    mailbox.post_rewrite(Rewrite::Rewind(7));
    mailbox.steer("after the rewind".into());
    assert!(
        matches!(drained(&inbox).as_slice(), [Item::Human(s), Item::Wakeup(_)] if s == "before")
    );
    assert!(drained(&inbox).is_empty());
    assert!(matches!(
        pop(&inbox),
        Some(Next::Rewrite(Rewrite::Rewind(7)))
    ));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "after the rewind"));
    assert!(inbox.is_empty());
}

#[test]
fn inbox_mid_exchange_drain_takes_reads_in_order_and_holds_at_a_rewrite() {
    let inbox = active();
    let mailbox = inbox.mailbox();
    mailbox.post_read(Read::Resources);
    mailbox.steer("a".into());
    mailbox.post_rewrite(Rewrite::Evict);
    mailbox.steer("b".into());
    assert!(
        matches!(inbox.drain().as_slice(), [Next::Read(Read::Resources), Next::Item(Item::Human(s))] if s == "a")
    );
    assert!(inbox.drain().is_empty());
    assert!(matches!(pop(&inbox), Some(Next::Rewrite(Rewrite::Evict))));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "b"));
    assert!(inbox.is_empty());
}

#[test]
fn a_held_rewrite_keeps_its_arrival_across_a_clear_boundary() {
    let inbox = active();
    inbox.mailbox().post_rewrite(Rewrite::Evict);
    let boundary = inbox.clear_boundary();
    assert!(inbox.drain().is_empty());
    inbox.apply_clear(boundary);
    assert!(inbox.is_empty());
}

#[test]
fn cancelled_exchange_drains_nothing() {
    let inbox = active();
    inbox.mailbox().steer("later".into());
    inbox.mailbox().interrupt();
    assert!(inbox.drain().is_empty());
    assert_eq!(inbox.mailbox().queued(), ["later"]);
}

#[test]
fn inbox_queued_shows_typed_text_in_order() {
    let inbox = inbox();
    let mailbox = inbox.mailbox();
    mailbox.post(wakeup(1, "morning", "@daily", "check"));
    mailbox.steer("first".into());
    mailbox.post_read(Read::Hatch {
        memory: Memory::Mnemon,
        name: Some("scout".into()),
    });
    mailbox.steer("second".into());
    assert_eq!(mailbox.queued(), ["first", "/branch scout", "second"]);
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Wakeup(_)))));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "first"));
    assert!(
        matches!(pop(&inbox), Some(Next::Read(Read::Hatch { name: Some(s), .. })) if s == "scout")
    );
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "second"));
}

#[test]
fn inbox_recall_no_user_prompts() {
    let inbox = inbox();
    inbox.mailbox().post(wakeup(1, "x", "@", "p"));
    assert_eq!(inbox.mailbox().recall(), [] as [String; 0]);
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Wakeup(_)))));
}

#[test]
fn inbox_recall_extracts_all_leaving_non_user_in_order() {
    let inbox = inbox();
    let mailbox = inbox.mailbox();
    mailbox.steer("first".into());
    mailbox.post(wakeup(1, "x", "@", "p"));
    mailbox.steer("second".into());
    mailbox.steer("third".into());
    mailbox.post_read(Read::Resources);
    mailbox.steer("fourth".into());
    assert_eq!(mailbox.recall(), ["first", "second\nthird", "fourth"]);
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Wakeup(_)))));
    assert!(matches!(pop(&inbox), Some(Next::Read(Read::Resources))));
    assert!(inbox.is_empty());
}

#[test]
fn abandon_applies_each_queue_policy_and_forgets_its_debts() {
    for queue in [Queue::Keep, Queue::Recall] {
        let inbox = active();
        let work = inbox.mailbox();
        let life = inbox.life_mailbox();
        let (child, _owing) = work.adopt("child".into(), Instant::now()).unwrap();
        let exchange = inbox.exchange();
        assert!(work.status().busy);
        work.steer("first".into());
        life.post(surface());
        work.steer("second".into());
        work.steer("third".into());
        work.post(wakeup(1, "x", "@", "p"));
        let recalled = inbox.abandon(queue);
        assert!(child.is_cancelled());
        assert!(exchange.is_cancelled());
        assert!(!work.is_current());
        assert!(inbox.mailbox().is_current());
        assert!(!life.status().busy);
        assert!(work.adopt("late".into(), Instant::now()).is_err());
        match queue {
            Queue::Keep => {
                assert_eq!(recalled, [] as [String; 0]);
                assert_eq!(life.queued(), ["first", "second\nthird"]);
                assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "first"));
                assert!(matches!(
                    pop(&inbox),
                    Some(Next::Item(Item::Surface { .. }))
                ));
                assert!(
                    matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "second\nthird")
                );
            }
            Queue::Recall => {
                assert_eq!(recalled, ["first", "second\nthird"]);
                assert_eq!(life.queued(), [] as [String; 0]);
                assert!(matches!(
                    pop(&inbox),
                    Some(Next::Item(Item::Surface { .. }))
                ));
            }
        }
        assert!(pop(&inbox).is_none(), "old work wakeup is fenced");
    }
}

#[test]
fn inbox_surface_drains_at_tool_boundary_and_discarded() {
    let inbox = active();
    inbox.life_mailbox().post(surface());
    inbox.apply_clear(inbox.clear_boundary());
    assert!(drained(&inbox).is_empty());
    inbox.mailbox().steer("next".into());
    open_next(&inbox);
    inbox.life_mailbox().post(surface());
    assert!(
        matches!(drained(&inbox).as_slice(), [Item::Surface { id, .. }] if *id == AgentId::new(0))
    );
}

#[test]
fn inbox_wakeup_leaves_the_queue_once_drained() {
    let inbox = inbox();
    let mailbox = inbox.mailbox();
    mailbox.post(wakeup(1, "n", "* * * * *", "go"));
    assert!(mailbox.has_queued_wakeup(ScheduleId::new(1)));
    assert!(!mailbox.has_queued_wakeup(ScheduleId::new(2)));
    pop(&inbox).unwrap();
    assert!(!mailbox.has_queued_wakeup(ScheduleId::new(1)));
}

#[test]
fn clear_leaves_no_wakeup_stranded_as_queued() {
    let inbox = inbox();
    let mailbox = inbox.mailbox();
    mailbox.post(wakeup(1, "n", "@", "go"));
    inbox.apply_clear(inbox.clear_boundary());
    assert!(!mailbox.has_queued_wakeup(ScheduleId::new(1)));
    assert!(inbox.is_empty());
}

#[test]
fn await_job_opens_for_a_prompt_even_behind_other_posts() {
    let inbox = active();
    inbox.nudge(1, "continue".into());
    inbox.mailbox().steer("hello".into());
    let job = inbox.await_job().unwrap();
    assert_eq!(job.id().get(), 1);
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Nudge { .. }))));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "hello"));
}

#[test]
fn await_job_returns_none_once_ended_even_with_a_prompt_queued() {
    let inbox = inbox();
    inbox.mailbox().steer("hello".into());
    inbox.mailbox().end(CancelCause::Cancelled);
    assert!(inbox.await_job().is_none());
    assert!(!inbox.is_empty());
}

#[test]
fn await_job_wakes_when_a_prompt_is_steered() {
    let inbox = inbox();
    let mailbox = inbox.mailbox();
    let (done, result) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || done.send(inbox.await_job()).unwrap());
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(20)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    mailbox.steer("hello".into());
    let outcome = result.recv_timeout(Duration::from_secs(5));
    mailbox.end(CancelCause::Cancelled);
    waiter.join().unwrap();
    assert!(outcome.unwrap().is_some());
}

fn depth_of(inbox: &Inbox, source: Source) -> u64 {
    inbox
        .source_depths()
        .into_iter()
        .find(|(s, _)| *s == source)
        .map_or(0, |(_, n)| n)
}

#[test]
fn inbox_scheduled_wakeup_dedupes_by_schedule_id_newest_wins() {
    let inbox = inbox();
    let mailbox = inbox.mailbox();
    mailbox.post(wakeup(1, "nightly", "@daily", "first"));
    mailbox.post(wakeup(2, "morning", "@daily", "other schedule"));
    mailbox.post(wakeup(1, "nightly", "@daily", "second"));
    assert_eq!(depth_of(&inbox, Source::Schedule), 2);
    assert!(
        matches!(pop(&inbox), Some(Next::Item(Item::Wakeup(s))) if s.contains("second") && !s.contains("first"))
    );
    assert!(
        matches!(pop(&inbox), Some(Next::Item(Item::Wakeup(s))) if s.contains("other schedule"))
    );
}

#[test]
fn inbox_user_steering_merges_pre_boundary_preserving_order() {
    let inbox = inbox();
    inbox.mailbox().steer("first line".into());
    inbox.mailbox().steer("second line".into());
    assert_eq!(depth_of(&inbox, Source::User), 1);
    assert!(
        matches!(pop(&inbox), Some(Next::Item(Item::Human(s))) if s == "first line\nsecond line")
    );
}

#[test]
fn inbox_nudge_replaces_a_still_queued_one_newest_wins() {
    let inbox = active();
    inbox.nudge(1, "retry".into());
    inbox.nudge(1, "retry".into());
    inbox.nudge(2, "different".into());
    assert_eq!(depth_of(&inbox, Source::Nudge), 1);
    assert!(
        matches!(pop(&inbox), Some(Next::Item(Item::Nudge { prompt: 2, text })) if text == "different")
    );
}

#[test]
fn esc_between_pop_and_open_cancels_the_opened_exchange() {
    let inbox = inbox();
    let before = inbox.exchange();
    inbox.mailbox().steer("work".into());
    let drawn = draw(&inbox).unwrap();
    inbox.mailbox().interrupt();
    drawn.opening.unwrap().open();
    let opened = inbox.exchange();
    assert_ne!(opened.id(), before.id());
    assert_eq!(opened.cause(), Some(CancelCause::Interrupted));
    assert_eq!(before.cause(), Some(CancelCause::Cancelled));
    assert!(inbox.mailbox().is_live());
}

#[test]
fn an_undrawn_opening_is_declined_on_drop() {
    let inbox = inbox();
    let before = inbox.exchange();
    inbox.mailbox().steer("work".into());
    let drawn = draw(&inbox).unwrap();
    assert!(inbox.0.lock().opening.is_some());
    drop(drawn);
    assert!(inbox.0.lock().opening.is_none());
    assert_eq!(inbox.exchange().id(), before.id());
}

#[test]
fn a_stale_opening_cannot_install_over_a_newer_one() {
    let inbox = inbox();
    let before = inbox.exchange();
    inbox.mailbox().steer("first".into());
    let stale = draw(&inbox).unwrap().opening.unwrap();
    inbox.mailbox().steer("second".into());
    let newer = draw(&inbox).unwrap().opening.unwrap();
    let minted = inbox.0.lock().opening.as_ref().unwrap().id();
    stale.open();
    assert_eq!(inbox.exchange().id(), before.id());
    newer.open();
    assert_eq!(inbox.exchange().id(), minted);
    assert!(inbox.0.lock().opening.is_none());
}

#[test]
fn idle_interrupt_fences_only_the_queued_nudge() {
    let inbox = active();
    inbox.nudge(1, "retry".into());
    inbox.life_mailbox().post(surface());
    inbox.mailbox().steer("next".into());
    inbox.mailbox().interrupt();
    assert!(matches!(
        pop(&inbox),
        Some(Next::Item(Item::Surface { .. }))
    ));
    assert!(matches!(open_next(&inbox), Next::Item(Item::Human(s)) if s == "next"));
    assert!(!inbox.exchange().is_cancelled());
    assert!(pop(&inbox).is_none());
}

#[test]
fn declined_surface_opens_nothing_and_preserves_a_queued_nudge() {
    let inbox = active();
    let exchange = inbox.exchange();
    inbox.life_mailbox().post(surface());
    inbox.nudge(1, "retry".into());
    assert!(matches!(
        pop(&inbox),
        Some(Next::Item(Item::Surface { .. }))
    ));
    assert_eq!(inbox.exchange().id(), exchange.id());
    assert!(!exchange.is_cancelled());
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Nudge { text, .. })) if text == "retry"));
}

#[test]
fn opening_an_exchange_and_rewind_fence_old_nudges() {
    for rewind in [false, true] {
        let inbox = active();
        inbox.mailbox().steer("next".into());
        inbox.nudge(1, "retry".into());
        if rewind {
            inbox.close_exchange();
        } else {
            open_next(&inbox);
        }
        if rewind {
            assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(_)))));
        }
        assert!(pop(&inbox).is_none());
    }
}

#[test]
fn busy_clears_exactly_when_the_paid_result_is_popped() {
    for drain in [false, true] {
        let inbox = active();
        let work = inbox.mailbox();
        let (_, (report, owed)) = work.adopt("worker".into(), Instant::now()).unwrap();
        assert!(work.status().busy);
        report.pay(AgentOutcome::Replied, [owed.into_receipt()]);
        assert!(work.status().busy);
        let next = if drain {
            inbox.drain().pop()
        } else {
            pop(&inbox)
        };
        assert!(matches!(next, Some(Next::Item(Item::Agent(_)))));
        assert!(!work.status().busy);
    }
}

#[test]
fn a_quiesce_verdict_cannot_beat_a_queued_result() {
    let inbox = inbox();
    inbox
        .mailbox()
        .post(result(AgentOutcome::Stopped("done".into())));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Agent(_)))));
    assert!(pop(&inbox).is_none());
}

#[test]
fn stopped_job_pops_nothing_and_preserves_life_and_peer_deliveries() {
    let (inbox, job) = running();
    let mailbox = inbox.mailbox();
    assert_eq!(mailbox.stop(job.id()), Some(vec!["running".into()]));
    inbox.life_mailbox().post(surface());
    let peer = DurableRoot::default().child();
    inbox
        .life_mailbox()
        .vouched_by((*peer).clone())
        .post(message());
    mailbox.steer("next".into());
    assert!(pop(&inbox).is_none());
    assert!(!inbox.is_empty());
    drop(stopped(job));
    let next = inbox.await_job().unwrap();
    assert!(matches!(
        pop(&inbox),
        Some(Next::Item(Item::Surface { .. }))
    ));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Message(_)))));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(text))) if text == "next"));
    assert!(matches!(next.close(), Closed::Ran));
}

#[test]
fn a_stopped_job_stays_open_until_its_second_settlement() {
    let (inbox, job) = running();
    let mailbox = inbox.mailbox();
    assert!(mailbox.stop(job.id()).is_some());
    mailbox.steer("next".into());
    let held = stopped(job);
    assert!(job_is_open(&inbox));
    assert!(pop(&inbox).is_none(), "a held job blocks draw");
    drop(held);
    assert!(!job_is_open(&inbox));
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(text))) if text == "next"));
}

#[test]
fn stop_racing_close_closes_the_ticket_exactly_once() {
    for _ in 0..64 {
        let (inbox, job) = running();
        let mailbox = inbox.mailbox();
        let id = job.id();
        let (recalled, closed) = std::thread::scope(|scope| {
            let stop = scope.spawn(|| mailbox.stop(id));
            let closed = job.close();
            (stop.join().unwrap(), closed)
        });
        assert_eq!(recalled.is_some(), matches!(closed, Closed::Stopped(_)));
        assert_eq!(job_is_open(&inbox), matches!(closed, Closed::Stopped(_)));
        drop(closed);
        assert!(!job_is_open(&inbox));
        mailbox.steer("again".into());
        assert_ne!(inbox.await_job().unwrap().id(), id, "ids are never reused");
    }
}

#[test]
fn a_panic_in_the_second_settlement_still_closes_the_ticket() {
    let (inbox, job) = running();
    assert!(inbox.mailbox().stop(job.id()).is_some());
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = stopped(job);
        panic!("second settlement failed");
    }));
    assert!(unwound.is_err());
    assert!(!job_is_open(&inbox));
}

#[test]
fn accepted_input_preceding_an_older_reply_prevents_lease_and_clean_retirement() {
    let parent = active();
    let (life, owing) = parent
        .mailbox()
        .adopt("worker".into(), Instant::now())
        .unwrap();
    let child = Inbox::new(life, Some(owing));
    let mailbox = child.mailbox();
    let owed = mailbox.report().and_then(|report| report.owe());
    assert!(mailbox.post(Scoped::Message {
        message: follow_up("follow up"),
        owed,
    }));
    assert!(child.deposit_reply(FOValue::Unit).is_none());
    assert!(mailbox.status().reply_since.is_none());
    assert!(mailbox.is_live());
    child.retire(AgentOutcome::Replied);
    assert!(!mailbox.is_live());
    assert!(!mailbox.post(message()));
    let busy = || parent.mailbox().status().busy;
    assert!(matches!(pop_outcome(&parent), AgentOutcome::Replied));
    assert!(busy(), "the accepted follow-up is still owed");
    assert!(matches!(pop_outcome(&parent), AgentOutcome::Cancelled));
    assert!(!busy());
}

#[test]
fn queued_human_input_blocks_the_lease() {
    let inbox = inbox();
    assert!(inbox.mailbox().steer("hi".into()));
    assert!(inbox.deposit_reply(FOValue::Unit).is_none());
    assert!(inbox.mailbox().status().reply_since.is_none());
    assert!(matches!(pop(&inbox), Some(Next::Item(Item::Human(_)))));
    assert!(inbox.deposit_reply(FOValue::Unit).is_some());
}

#[test]
fn observers_never_see_a_dead_post() {
    let inbox = active();
    let from = DurableRoot::default().child();
    let vouched = inbox.mailbox().vouched_by((*from).clone());
    vouched.post(surface());
    vouched.post(wakeup(1, "tick", "@", "go"));
    assert_eq!(depth_of(&inbox, Source::Surface), 1);
    assert!(vouched.has_queued_wakeup(ScheduleId::new(1)));
    from.cancel(CancelCause::Cancelled);
    assert!(!vouched.has_queued_wakeup(ScheduleId::new(1)));
    assert_eq!(depth_of(&inbox, Source::Surface), 0);
    assert_eq!(depth_of(&inbox, Source::Schedule), 0);
    assert!(inbox.is_empty());
}
