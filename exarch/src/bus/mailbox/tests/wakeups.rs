use super::*;

pub(super) fn book(mailbox: &AgentMailbox, label: &str) {
    assert!(
        mailbox
            .transact(|txn| txn.schedule(
                label.into(),
                Schedule {
                    prompt: "wake".into(),
                    trigger: "* * * * *".into(),
                    cron: Some("* * * * *".into()),
                    next_unix_ms: 60_000,
                    fires: 0,
                }
            ))
            .unwrap()
            .unwrap()
    );
}

#[test]
fn a_fire_coalesces_per_schedule_id_and_a_cleared_schedule_does_not_fire() {
    let mailbox = mailbox(Admission::Free);
    let id = ScheduleId::new(1);
    book(&mailbox, "one");
    book(&mailbox, "other");
    for text in ["first", "coalesced"] {
        assert!(
            mailbox
                .transact(|txn| txn.fire("one", id, Some(120_000), text.into()))
                .unwrap()
                .unwrap()
        );
    }
    mailbox
        .transact(|txn| txn.fire("other", ScheduleId::new(2), Some(120_000), "other".into()))
        .unwrap()
        .unwrap();
    assert_eq!(depth(&mailbox, Source::Schedule), 2);
    assert!(
        matches!(take(&mailbox).0, Input::Wakeup { id: received, text }
        if received == id && text == "first")
    );
    mailbox
        .transact(|txn| txn.unschedule("one"))
        .unwrap()
        .unwrap();
    #[allow(
        clippy::redundant_closure_for_method_calls,
        reason = "the method item is not general enough for transact's higher-ranked lifetime"
    )]
    mailbox.transact(|txn| txn.clear()).unwrap().unwrap();
    assert!(
        !mailbox
            .transact(|txn| txn.fire("other", ScheduleId::new(2), None, "retired".into()))
            .unwrap()
            .unwrap()
    );
    assert_eq!(depth(&mailbox, Source::Schedule), 0);
}

#[test]
fn serialized_fires_enqueue_only_one_delivery_for_the_same_id() {
    for _ in 0..32 {
        let mailbox = mailbox(Admission::Free);
        book(&mailbox, "one");
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let fire = || {
                barrier.wait();
                mailbox
                    .transact(|txn| {
                        txn.fire(
                            "one",
                            ScheduleId::new(1),
                            Some(120_000),
                            "one delivery".into(),
                        )
                    })
                    .unwrap()
                    .unwrap();
            };
            let first = scope.spawn(fire);
            fire();
            first.join().unwrap();
        });
        assert_eq!(depth(&mailbox, Source::Schedule), 1);
        assert!(matches!(take(&mailbox).0, Input::Wakeup { text, .. } if text == "one delivery"));
        assert_eq!(depth(&mailbox, Source::Schedule), 0);
    }
}

#[test]
fn an_ended_mailbox_drops_a_ring() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.end(CancelCause::Cancelled);
    address.ring("one".into(), ScheduleId::new(1));
    #[allow(
        clippy::redundant_closure_for_method_calls,
        reason = "the method item is not general enough for transact's higher-ranked lifetime"
    )]
    let rung = address.transact(|txn| txn.rung()).unwrap();
    assert!(rung.is_empty());
}
