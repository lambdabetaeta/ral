use super::*;

fn last(journal: &Recorder) -> u64 {
    journal
        .reader()
        .since(None)
        .unwrap()
        .2
        .last()
        .unwrap()
        .unwrap()
        .number()
}

#[test]
fn post_command_recall_and_stop_return_only_after_sync_outside_the_cell() {
    let dir = OwnDir::for_test("mailbox-sync-boundaries").unwrap();
    let journal = Recorder::create(&dir).unwrap();
    let mailbox = recorded_mailbox(Admission::Free, journal.clone());
    let address = mailbox.address();
    let observing = address.clone();
    journal.before_sync(move || {
        assert!(
            observing.status().is_ok(),
            "post released its cell before syncing"
        );
        Ok(())
    });
    address.steer("first".into()).unwrap();
    journal.assert_synced_through(last(&journal));
    let observing = address.clone();
    journal.before_sync(move || {
        assert!(
            observing.status().is_ok(),
            "command released its cell before syncing"
        );
        Ok(())
    });
    address.command(Command::Read(Read::Resources)).unwrap();
    journal.assert_synced_through(last(&journal));
    let observing = address.clone();
    journal.before_sync(move || {
        assert!(
            observing.status().is_ok(),
            "recall released its cell before syncing"
        );
        Ok(())
    });
    assert_eq!(address.recall().unwrap(), ["first"]);
    journal.assert_synced_through(last(&journal));
    assert!(matches!(
        mailbox.next(QUIET),
        Next::Command(Command::Read(Read::Resources))
    ));
    let _exchange = start(&mailbox);
    address.steer("withdraw".into()).unwrap();
    let observing = address.clone();
    journal.before_sync(move || {
        assert!(
            observing.status().is_ok(),
            "Stop released its cell before syncing"
        );
        Ok(())
    });
    assert_eq!(
        address
            .stop(address.status().unwrap().job.unwrap())
            .unwrap(),
        Some(vec!["withdraw".into()])
    );
    journal.assert_synced_through(last(&journal));
}

#[test]
fn a_post_sync_failure_is_queued_possibly_read_and_never_safe_to_resend() {
    let dir = OwnDir::for_test("post-unsynced").unwrap();
    let journal = Recorder::create(&dir).unwrap();
    let mailbox = recorded_mailbox(Admission::Free, journal.clone());
    let address = mailbox.address();
    journal.before_sync(|| Err(std::io::Error::other("injected sync failure")));
    let error = address.steer("accepted".into()).unwrap_err();
    assert!(matches!(error, Refused::Unsynced(_)));
    let message = error.to_string();
    assert!(message.contains("queued and possibly already read"));
    assert!(message.contains("do not automatically resend"));
    assert!(!address.is_live());
    assert_eq!(
        address.queued_posts().len(),
        1,
        "the accepted post was not rolled back"
    );
    assert!(address.recall().is_err(), "failure is not an empty recall");
    assert!(
        address.stop(1.into()).is_err(),
        "failure is not a stale Stop"
    );
}

#[test]
fn recall_and_stop_do_not_swallow_failed_withdrawal_or_sync() {
    for stop in [false, true] {
        let dir = OwnDir::for_test("withdraw-unsynced").unwrap();
        let journal = Recorder::create(&dir).unwrap();
        let mailbox = recorded_mailbox(Admission::Free, journal.clone());
        let exchange = start(&mailbox);
        let address = mailbox.address();
        address.steer("queued".into()).unwrap();
        journal.before_sync(|| Err(std::io::Error::other("withdraw sync failed")));
        if stop {
            assert!(
                address
                    .stop(address.status().unwrap().job.unwrap())
                    .is_err()
            );
            assert!(exchange.is_cancelled());
        } else {
            assert!(address.recall().is_err());
        }
        assert!(
            address.queued_posts().is_empty(),
            "withdrawal already committed"
        );
    }
}

#[test]
fn report_syncs_its_status_before_parent_delivery_and_parent_sync() {
    let parent_dir = OwnDir::for_test("report-parent-sync").unwrap();
    let helper_dir = OwnDir::for_test("report-helper-sync").unwrap();
    let parent_journal = Recorder::create(&parent_dir).unwrap();
    let parent = recorded_mailbox(Admission::Free, parent_journal.clone());
    let helper_journal = Recorder::create(&helper_dir).unwrap();
    let helper = configured_mailbox(
        Admission::Parent { idle: WAIT },
        helper_journal.clone(),
        parent.life().child(),
        Some(parent.address()),
        true,
    );
    helper.address().post(task("report")).unwrap();
    let status_commit = last(&helper_journal) + 1;
    let witness = helper_journal.clone();
    parent_journal.before_sync(move || {
        witness.assert_synced_through(status_commit);
        Ok(())
    });
    helper
        .address()
        .report(ChildStatus::Replied(FOValue::Unit), child)
        .unwrap();
    helper_journal.assert_synced_through(status_commit);
    parent_journal.assert_synced_through(last(&parent_journal));
    assert!(matches!(
        take(&parent).0,
        Input::Child {
            status: ChildStatus::Replied(_),
            ..
        }
    ));
}

#[test]
fn failed_status_sync_uses_precommit_debt_and_sends_only_one_failed_fallback() {
    let dir = OwnDir::for_test("report-status-unsynced").unwrap();
    let parent = mailbox(Admission::Free);
    let journal = Recorder::create(&dir).unwrap();
    let helper = configured_mailbox(
        Admission::Parent { idle: WAIT },
        journal.clone(),
        parent.life().child(),
        Some(parent.address()),
        true,
    );
    helper.address().post(task("owed")).unwrap();
    let (input, _exchange) = take(&helper);
    assert!(matches!(input, Input::Task(message) if message.text == "owed"));
    assert!(helper.address().status().unwrap().owes);
    journal.before_sync(|| Err(std::io::Error::other("status sync failed")));
    assert!(
        helper
            .address()
            .report(ChildStatus::Replied(FOValue::Unit), child)
            .is_err()
    );
    assert!(
        !helper.address().status().unwrap().owes,
        "Status cleared the fold's debt before sync failed"
    );
    assert!(
        helper
            .address()
            .report(ChildStatus::Stopped, child)
            .is_err()
    );
    assert_eq!(parent.address().queued_posts().len(), 1);
    assert!(
        matches!(take(&parent).0, Input::Child { status: ChildStatus::Failed(reason), .. } if reason.contains("status sync failed"))
    );
}

#[test]
fn a_parentless_report_still_syncs_status() {
    let dir = OwnDir::for_test("parentless-status-sync").unwrap();
    let journal = Recorder::create(&dir).unwrap();
    let mailbox = recorded_mailbox(Admission::Free, journal.clone());
    mailbox
        .address()
        .report(ChildStatus::Stopped, child)
        .unwrap();
    journal.assert_synced_through(last(&journal));
}
