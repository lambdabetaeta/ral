use super::*;
use crate::record::Replay;
use crate::schedule::{CronSchedule, ScheduleRegistry, Trigger};
use jiff::Timestamp;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn queue_is_mail(mailbox: &AgentMailbox, seed: u64, turn: usize) {
    let pending: Vec<_> = mailbox
        .transact(|txn| txn.mail().pending().map(|(id, _)| id).collect())
        .unwrap();
    assert_eq!(
        mailbox.address().queued_posts(),
        pending,
        "seed {seed}, turn {turn}"
    );
}

fn replay_is_cell(mailbox: &AgentMailbox, journal: &Recorder) {
    let (_, commits) = journal.reader().since(None).unwrap();
    let records: Vec<_> = commits
        .map(Result::unwrap)
        .flat_map(|commit| commit.records().iter().cloned().collect::<Vec<_>>())
        .collect();
    let mut replay = Replay::open(&records[0]).unwrap();
    for record in &records[1..] {
        replay.step(record).unwrap();
    }
    let (mail, schedules, model) = replay.finish();
    mailbox
        .transact(|txn| {
            assert_eq!(&mail, txn.mail());
            assert_eq!(&schedules, txn.schedules());
            assert_eq!(&model, txn.model());
        })
        .unwrap();
}

fn recurring() -> Trigger {
    Trigger::Cron {
        schedule: CronSchedule::parse("* * * * *").unwrap(),
        expr: "* * * * *".into(),
    }
}

fn due(mailbox: &AgentMailbox, label: &str) -> Timestamp {
    mailbox
        .transact(|txn| {
            txn.schedules()
                .get(label)
                .map_or_else(Timestamp::now, |schedule| {
                    Timestamp::from_millisecond(i64::try_from(schedule.next_unix_ms).unwrap())
                        .unwrap()
                })
        })
        .unwrap()
}

#[test]
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "method items do not satisfy transact's higher-ranked lifetime"
)]
fn random_cell_operations_keep_queue_equal_to_mail_and_file_replay_equal_to_cell() {
    // Each shuffled epoch visits every operation, not just whichever a seed happens to choose.
    for seed in [1_u64, 7, 23, 0x5eed, 0xdead_beef, u64::MAX] {
        for admission in [
            Admission::Free,
            Admission::Gated,
            Admission::Parent {
                idle: Duration::from_hours(1),
            },
        ] {
            let parent_dir = OwnDir::for_test("queue-property-parent").unwrap();
            let parent_journal = Recorder::create(&parent_dir).unwrap();
            let parent = recorded_mailbox(Admission::Free, parent_journal.clone());
            let dir = OwnDir::for_test("queue-property").unwrap();
            let journal = Recorder::create(&dir).unwrap();
            let mailbox = configured_mailbox(
                admission,
                journal.clone(),
                parent.life().child(),
                matches!(admission, Admission::Parent { .. }).then(|| parent.address()),
                true,
            );
            let address = mailbox.address();
            let mut registry = ScheduleRegistry::new();
            let mut random = seed;
            let mut visits = [0; 29];
            let mut turn = 0;
            macro_rules! step {
                ($operation:expr) => {{
                    let result = $operation;
                    queue_is_mail(&mailbox, seed, turn);
                    queue_is_mail(&parent, seed, turn);
                    turn += 1;
                    result
                }};
            }
            if matches!(admission, Admission::Parent { .. }) {
                let before = std::fs::read(dir.path().join(RECORD)).unwrap();
                assert!(matches!(
                    step!(address.post(message("resting peer", false))),
                    Err(Refused::Resting)
                ));
                assert_eq!(std::fs::read(dir.path().join(RECORD)).unwrap(), before);
            }
            for _ in 0..6 {
                let mut operations: Vec<_> = (0..visits.len()).collect();
                for i in (1..operations.len()).rev() {
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    let j = usize::try_from(random % u64::try_from(i + 1).unwrap()).unwrap();
                    operations.swap(i, j);
                }
                for operation in operations {
                    visits[operation] += 1;
                    match operation {
                        0 => {
                            step!(address.steer(format!("human {turn}"))).unwrap();
                        }
                        1 | 2 => {
                            let _refusal = step!(address.post(message("message", operation == 1)));
                        }
                        3 => {
                            step!(address.post(child(ChildStatus::Stopped))).unwrap();
                        }
                        4 => {
                            step!(address.post(wakeup(99, "direct wakeup"))).unwrap();
                        }
                        5 => {
                            step!(address.post(surface())).unwrap();
                        }
                        6 => {
                            step!(address.command(Command::Read(Read::Resources))).unwrap();
                        }
                        7 => {
                            step!(address.command(Command::Rewrite(Rewrite::Clear))).unwrap();
                        }
                        8 => {
                            step!(address.command(Command::Model(super::queue::model_pick())))
                                .unwrap();
                        }
                        9 => {
                            step!(address.gone(AgentId::new(11)));
                        }
                        10 => {
                            // A command bounds next even when admission would hold all prompts.
                            step!(address.command(Command::Read(Read::Context))).unwrap();
                            let _next = step!(mailbox.next(QUIET));
                        }
                        11 => {
                            assert!(step!(mailbox.begin()));
                        }
                        12 => {
                            step!(mailbox.transact(|txn| txn.clear())).unwrap().unwrap();
                            registry.clear();
                            step!(mailbox.transact(|txn| registry.fire(txn, Timestamp::now())))
                                .unwrap()
                                .unwrap();
                            assert!(step!(mailbox.begin()));
                            let budget = step!(address.queued_posts()).len() + 2;
                            let mut settled = false;
                            for _ in 0..budget {
                                if matches!(step!(mailbox.next(QUIET)), Next::Settled { .. }) {
                                    settled = true;
                                    break;
                                }
                            }
                            assert!(
                                settled,
                                "seed {seed}: next must settle within its queue bound"
                            );
                        }
                        13 => {
                            let _drained = step!(mailbox.drain());
                        }
                        14 => {
                            step!(address.steer("recall target".into())).unwrap();
                            let recalled = step!(address.recall());
                            assert!(recalled.last().unwrap().ends_with("recall target"));
                        }
                        15 => {
                            assert!(step!(mailbox.begin()));
                            step!(address.steer("stop target".into())).unwrap();
                            let job = step!(address.status()).unwrap().job.unwrap();
                            let recalled = step!(address.stop(job)).unwrap();
                            assert!(recalled.last().unwrap().ends_with("stop target"));
                        }
                        16 => {
                            assert!(step!(mailbox.begin()));
                            assert!(step!(address.stop_current()));
                        }
                        17 => {
                            step!(publish(&address, ChildStatus::Replied(FOValue::Unit)));
                            registry.clear();
                        }
                        18 => {
                            let _booking = step!(mailbox.transact(|txn| registry.schedule(
                                txn,
                                recurring(),
                                "cron".into(),
                                "clock".into()
                            )))
                            .unwrap();
                        }
                        19 => {
                            let _booking = step!(mailbox.transact(|txn| registry.schedule(
                                txn,
                                Trigger::After(Duration::from_hours(2)),
                                "once".into(),
                                "once".into()
                            )))
                            .unwrap();
                        }
                        20 => {
                            let _booking = step!(mailbox.transact(|txn| registry.schedule(
                                txn,
                                recurring(),
                                "remove target".into(),
                                "clock".into()
                            )))
                            .unwrap();
                            assert!(
                                step!(mailbox.transact(|txn| registry.unschedule(txn, "clock")))
                                    .unwrap()
                                    .unwrap()
                            );
                        }
                        21 => {
                            step!(registry.ring("clock", &address));
                            step!(registry.ring("once", &address));
                        }
                        22 => {
                            let now = step!(due(&mailbox, "clock"));
                            step!(mailbox.transact(|txn| registry.fire(txn, now)))
                                .unwrap()
                                .unwrap();
                        }
                        23 => {
                            step!(mailbox.transact(|txn| txn.clear())).unwrap().unwrap();
                            registry.clear();
                        }
                        24 => {
                            step!(mailbox.transact(|txn| {
                                txn.change_model(Selected::for_test(&format!("model-{turn}")))
                            }))
                            .unwrap()
                            .unwrap();
                        }
                        25 => {
                            assert!(step!(address.release()));
                        }
                        26 => {
                            // Force both a due fire and a due queued overlap in every epoch.
                            let _booking = step!(mailbox.transact(|txn| registry.schedule(
                                txn,
                                recurring(),
                                "overlap".into(),
                                "clock".into()
                            )))
                            .unwrap();
                            for _ in 0..2 {
                                let now = step!(due(&mailbox, "clock"));
                                step!(registry.ring("clock", &address));
                                step!(mailbox.transact(|txn| registry.fire(txn, now)))
                                    .unwrap()
                                    .unwrap();
                            }
                        }
                        27 => {
                            // Append a human run through Gone, then take it in a bounded prefix.
                            step!(address.steer("coalesce a".into())).unwrap();
                            step!(address.gone(AgentId::new(12)));
                            step!(address.steer("coalesce b".into())).unwrap();
                            assert!(step!(mailbox.begin()));
                            let budget =
                                step!(mailbox.transact(|txn| txn.cell.queue.len())).unwrap() + 1;
                            let mut taken = false;
                            for _ in 0..budget {
                                match step!(mailbox.next(QUIET)) {
                                    Next::Rung => {
                                        step!(
                                            mailbox.transact(
                                                |txn| registry.fire(txn, Timestamp::now())
                                            )
                                        )
                                        .unwrap()
                                        .unwrap();
                                    }
                                    Next::Take {
                                        input: Input::Human(text),
                                        ..
                                    } if text.ends_with("coalesce a\ncoalesce b") => {
                                        taken = true;
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            assert!(
                                taken,
                                "seed {seed}: the bounded prefix must take the human run"
                            );
                        }
                        28 => {
                            let _booking = step!(mailbox.transact(|txn| registry.schedule(
                                txn,
                                Trigger::After(Duration::from_hours(2)),
                                "one-shot fire".into(),
                                "once".into()
                            )))
                            .unwrap();
                            let now = step!(due(&mailbox, "once"));
                            step!(registry.ring("once", &address));
                            step!(mailbox.transact(|txn| registry.fire(txn, now)))
                                .unwrap()
                                .unwrap();
                        }
                        _ => unreachable!(),
                    }
                }
            }
            assert!(visits.iter().all(|visits| *visits == 6));
            // Refusals of every payload kind and commands leave the file and queue untouched.
            step!(address.end(CancelCause::Cancelled));
            let before = std::fs::read(dir.path().join(RECORD)).unwrap();
            for input in [
                Input::Human("late".into()),
                message("parent", true),
                message("peer", false),
                child(ChildStatus::Stopped),
                wakeup(100, "late"),
                surface(),
            ] {
                assert!(matches!(step!(address.post(input)), Err(Refused::Ended)));
            }
            for command in [
                Command::Read(Read::Context),
                Command::Rewrite(Rewrite::Clear),
                Command::Model(super::queue::model_pick()),
            ] {
                assert!(matches!(
                    step!(address.command(command)),
                    Err(Refused::Ended)
                ));
            }
            step!(address.gone(AgentId::new(11)));
            step!(registry.ring("clock", &address));
            assert!(matches!(step!(mailbox.next(QUIET)), Next::Ended));
            assert_eq!(std::fs::read(dir.path().join(RECORD)).unwrap(), before);
            step!(publish(&address, ChildStatus::Failed("ended".into())));
            assert!(turn >= 29 * 6);
            replay_is_cell(&mailbox, &journal);
            replay_is_cell(&parent, &parent_journal);
        }
    }
}

#[test]
fn gone_does_not_split_human_posts_in_queue_next_or_drain() {
    for drain in [false, true] {
        let mailbox = mailbox(Admission::Free);
        let address = mailbox.address();
        address.steer("a".into()).unwrap();
        address.gone(AgentId::new(8));
        address.steer("b".into()).unwrap();
        assert_eq!(address.queued(), ["a\nb"]);
        assert_eq!(depth(&mailbox, Source::User), 1);
        if drain {
            assert!(matches!(mailbox.drain().as_slice(),
                [Drained::Input(Input::Human(text)), Drained::Gone(id)]
                if text == "a\nb" && *id == AgentId::new(8)));
        } else {
            assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "a\nb"));
            assert!(matches!(mailbox.next(QUIET), Next::Gone(id) if id == AgentId::new(8)));
        }
        queue_is_mail(&mailbox, 0, 0);
    }
}

#[test]
fn a_wakeup_is_taken_alone_before_a_coalesced_human_run() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.post(wakeup(0, "tick")).unwrap();
    address.steer("a".into()).unwrap();
    address.steer("b".into()).unwrap();
    assert!(matches!(take(&mailbox).0, Input::Wakeup { text, .. } if text == "tick"));
    assert_eq!(address.queued(), ["a\nb"]);
    assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "a\nb"));
}

#[test]
fn a_poisoned_cell_latches_an_actionable_failure_and_refuses_posts() {
    let mailbox = helper();
    let address = mailbox.address();
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let _result = address.transact(|_| panic!("transaction panic"));
        }))
        .is_err()
    );
    let expected = format!(
        "a thread panicked inside agent {}'s transaction, so its state may no longer match its log; it has ended, and resuming the run rebuilds its state from the log",
        address.0.id
    );
    assert_eq!(address.transact(|_| ()).unwrap_err().to_string(), expected);
    assert_eq!(address.status().err().unwrap().to_string(), expected);
    assert!(!address.is_live());
    assert!(matches!(address.post(surface()), Err(Refused::Ended)));
    assert!(matches!(mailbox.next(QUIET), Next::Ended));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "re-entered agent 7's transaction")]
fn reentering_the_same_cell_names_the_agent_instead_of_deadlocking() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address
        .transact(|_| {
            let _status = address.status();
        })
        .unwrap();
}

#[test]
fn release_while_working_does_not_release_the_next_gated_job() {
    let mailbox = mailbox(Admission::Gated);
    let address = mailbox.address();
    address.release();
    let _exchange = start(&mailbox);
    assert!(address.release());
    settled(&mailbox, false);
    address.steer("held".into()).unwrap();
    // An always-admitted command bounds next and exposes any leaked release.
    address.command(Command::Read(Read::Resources)).unwrap();
    assert!(matches!(
        mailbox.next(QUIET),
        Next::Command(Command::Read(Read::Resources))
    ));
    assert_eq!(address.queued(), ["held"]);
    assert!(address.status().unwrap().job.is_none());
    assert!(address.release());
    assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "held"));
}

#[test]
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "method items do not satisfy transact's higher-ranked lifetime"
)]
fn clear_records_one_combined_withdrawal_and_all_unschedules_preserving_typed_posts() {
    let dir = OwnDir::for_test("clear-commit").unwrap();
    let journal = Recorder::create(&dir).unwrap();
    let mailbox = recorded_mailbox(Admission::Free, journal.clone());
    let address = mailbox.address();
    for label in ["one", "two"] {
        super::wakeups::book(&mailbox, label);
    }
    address.steer("keep human".into()).unwrap();
    address.post(message("keep message", false)).unwrap();
    address.command(Command::Read(Read::Context)).unwrap();
    let keep = address.queued_posts();
    address.post(wakeup(0, "withdraw")).unwrap();
    address.post(child(ChildStatus::Stopped)).unwrap();
    address.post(surface()).unwrap();
    let withdrawn = address.queued_posts()[keep.len()..].to_vec();
    address.gone(AgentId::new(8));
    mailbox.transact(|txn| txn.clear()).unwrap().unwrap();
    assert_eq!(address.queued_posts(), keep);
    let (_, commits) = journal.reader().since(None).unwrap();
    let last = commits.map(Result::unwrap).last().unwrap();
    assert!(
        matches!(last.records().iter().map(crate::record::Recorded::value).collect::<Vec<_>>().as_slice(),
        [Record::Control(Control::Withdrawn { posts }),
         Record::Control(Control::Unscheduled { label: one }),
         Record::Control(Control::Unscheduled { label: two })]
        if posts.iter().copied().collect::<Vec<_>>() == withdrawn && one == "one" && two == "two")
    );
    assert!(matches!(mailbox.drain().as_slice(),
        [Drained::Input(Input::Human(text)), Drained::Input(Input::Message(_)), Drained::Read(Read::Context)]
        if text == "keep human"));
}
