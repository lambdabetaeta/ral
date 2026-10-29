use super::*;

fn report(helper: &AgentAddress, parent: &AgentAddress, status: ChildStatus) {
    helper.report(status.clone(), Some((parent, child(status))));
}

#[test]
fn a_helpers_birth_report_is_published_and_taken_up_in_the_parents_job() {
    for status in [
        ChildStatus::Replied(FOValue::Unit),
        ChildStatus::Failed("failed".into()),
        ChildStatus::Stopped,
    ] {
        let parent = mailbox(Admission::Free);
        let _exchange = start(&parent);
        let job = parent.address().status().job;
        let helper = helper();
        let address = helper.address();
        assert!(address.status().owes_parent);
        report(&address, &parent.address(), status.clone());
        assert_eq!(address.status().report, Some(status.clone()));
        assert!(!address.status().owes_parent);
        assert!(!address.status().working());
        assert!(
            matches!(take(&parent).0, Input::Child { id, name, status: received, elapsed }
            if id == AgentId::new(8) && &*name == "helper" && received == status && elapsed == Duration::from_secs(2))
        );
        assert_eq!(parent.address().status().job, job);
        settled(&parent, false);
    }
}

#[test]
fn a_follow_up_crossing_a_report_keeps_owing_until_the_follow_up_reports() {
    for _ in 0..32 {
        let parent = mailbox(Admission::Free);
        let _exchange = start(&parent);
        let helper = helper();
        let _helper_exchange = start(&helper);
        let address = helper.address();
        let parent_address = parent.address();
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let follow_up = scope.spawn(|| {
                barrier.wait();
                address.post(message("follow up", true)).unwrap();
            });
            barrier.wait();
            report(
                &address,
                &parent_address,
                ChildStatus::Replied(FOValue::Unit),
            );
            follow_up.join().unwrap();
        });
        assert!(address.status().owes_parent);
        assert!(matches!(
            take(&parent).0,
            Input::Child {
                status: ChildStatus::Replied(_),
                ..
            }
        ));
        let (input, _exchange) = take(&helper);
        assert!(matches!(input, Input::Message(message) if message.text == "follow up"));
        assert!(
            address.status().owes_parent,
            "taking input does not discharge its report"
        );
        report(&address, &parent_address, ChildStatus::Stopped);
        assert!(!address.status().owes_parent);
        assert!(matches!(
            take(&parent).0,
            Input::Child {
                status: ChildStatus::Stopped,
                ..
            }
        ));
        settled(&parent, false);
    }
}

#[test]
fn a_crossing_follow_up_keeps_the_parent_job_open_until_its_report_wakes_it() {
    let parent = mailbox(Admission::Free);
    let _exchange = start(&parent);
    let parent_address = parent.address();
    let job = parent_address.status().job;
    let helper = helper();
    let address = helper.address();
    address.post(message("follow up", true)).unwrap();
    report(
        &address,
        &parent_address,
        ChildStatus::Replied(FOValue::Unit),
    );
    let (_report, _exchange) = take(&parent);
    let (_follow_up, _exchange) = take(&helper);
    let next = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            parent.next(&Rest {
                owing: address.status().owes_parent,
                lives: Vec::new(),
                schedules: false,
            })
        });
        let waiting = wait_until(|| parent_address.status().busy);
        let still_open = parent_address.status().job == job && !worker.is_finished();
        report(&address, &parent_address, ChildStatus::Stopped);
        let finished = wait_until(|| worker.is_finished());
        if !finished {
            parent_address.end(CancelCause::Cancelled);
        }
        let next = worker.join().unwrap();
        assert!(waiting);
        assert!(still_open);
        assert!(finished);
        next
    });
    assert!(matches!(
        next,
        Next::Take {
            input: Input::Child {
                status: ChildStatus::Stopped,
                ..
            },
            ..
        }
    ));
    assert_eq!(parent_address.status().job, job);
    settled(&parent, false);
}

#[test]
fn queued_parent_messages_keep_owing_through_multiple_reports() {
    let parent = mailbox(Admission::Free);
    let _exchange = start(&parent);
    let helper = helper();
    let address = helper.address();
    address.post(message("first", true)).unwrap();
    address.post(message("second", true)).unwrap();
    report(
        &address,
        &parent.address(),
        ChildStatus::Replied(FOValue::Unit),
    );
    assert!(address.status().owes_parent);
    let (_input, _exchange) = take(&helper);
    report(&address, &parent.address(), ChildStatus::Stopped);
    assert!(address.status().owes_parent);
    let (_input, _exchange) = take(&helper);
    report(
        &address,
        &parent.address(),
        ChildStatus::Failed("done".into()),
    );
    assert!(!address.status().owes_parent);
    assert_eq!(depth(&parent, Source::Agent), 3);
}

#[test]
fn a_parent_waking_after_report_publication_cannot_settle_with_its_report_unread() {
    for _ in 0..32 {
        let parent = mailbox(Admission::Free);
        let _exchange = start(&parent);
        let helper = helper();
        let address = helper.address();
        let parent_address = parent.address();
        let barrier = Barrier::new(2);
        let next = std::thread::scope(|scope| {
            let reporting = scope.spawn(|| {
                barrier.wait();
                report(
                    &address,
                    &parent_address,
                    ChildStatus::Replied(FOValue::Unit),
                );
            });
            barrier.wait();
            let owing = address.status().owes_parent;
            let owner = &parent;
            let worker = scope.spawn(move || {
                owner.next(&Rest {
                    owing,
                    lives: Vec::new(),
                    schedules: false,
                })
            });
            reporting.join().unwrap();
            let finished = wait_until(|| worker.is_finished());
            if !finished {
                parent_address.end(CancelCause::Cancelled);
            }
            let next = worker.join().unwrap();
            assert!(finished, "publishing a report must wake the parent");
            next
        });
        assert!(matches!(
            next,
            Next::Take {
                input: Input::Child { .. },
                ..
            }
        ));
        assert!(parent_address.status().job.is_some());
        settled(&parent, false);
    }
}

#[test]
fn an_ended_helper_can_still_publish_the_failure_it_owes() {
    let parent = mailbox(Admission::Free);
    let _exchange = start(&parent);
    let helper = helper();
    let address = helper.address();
    report(
        &address,
        &parent.address(),
        ChildStatus::Replied(FOValue::Unit),
    );
    let (_input, _exchange) = take(&parent);
    address.post(message("queued follow up", true)).unwrap();
    address.end(CancelCause::Cancelled);
    report(
        &address,
        &parent.address(),
        ChildStatus::Failed("ended before reporting".into()),
    );
    assert!(
        matches!(take(&parent).0, Input::Child { status: ChildStatus::Failed(reason), .. } if reason == "ended before reporting")
    );
    assert_eq!(
        address.status().report,
        Some(ChildStatus::Failed("ended before reporting".into()))
    );
}

#[test]
fn a_human_resuming_a_helper_publishes_status_without_notifying_its_parent() {
    for admission in [
        Admission::Free,
        Admission::Gated,
        Admission::Parent { idle: WAIT },
    ] {
        let parent = mailbox(admission);
        let parent_address = parent.address();
        let helper = helper();
        let address = helper.address();
        report(
            &address,
            &parent_address,
            ChildStatus::Replied(FOValue::Unit),
        );
        let (_birth_report, _exchange) = take(&parent);
        assert!(!address.status().working());
        address.steer("human follow up".into()).unwrap();
        let (_input, _exchange) = take(&helper);
        assert!(helper.begin());
        assert!(address.status().working());
        assert!(!address.status().owes_parent);
        report(&address, &parent_address, ChildStatus::Stopped);
        settled(&helper, false);
        assert_eq!(address.status().report, Some(ChildStatus::Stopped));
        assert!(!address.status().working());
        assert_eq!(depth(&parent, Source::Agent), 0);
        assert!(parent_address.status().job.is_none());
    }
}

#[test]
fn peer_messages_in_a_working_helper_do_not_create_a_parent_obligation() {
    let parent = mailbox(Admission::Free);
    let helper = helper();
    let address = helper.address();
    report(
        &address,
        &parent.address(),
        ChildStatus::Replied(FOValue::Unit),
    );
    let (_input, _exchange) = take(&parent);
    let _exchange = start(&helper);
    address.post(message("peer", false)).unwrap();
    assert!(!address.status().owes_parent);
    report(&address, &parent.address(), ChildStatus::Stopped);
    assert_eq!(depth(&parent, Source::Agent), 0);
    assert_eq!(address.status().report, Some(ChildStatus::Stopped));
}

#[test]
fn reporting_to_an_ended_parent_still_updates_the_helpers_published_status() {
    let parent = mailbox(Admission::Free);
    let helper = helper();
    parent.address().end(CancelCause::Cancelled);
    report(&helper.address(), &parent.address(), ChildStatus::Stopped);
    assert_eq!(helper.address().status().report, Some(ChildStatus::Stopped));
    assert!(!helper.address().status().owes_parent);
}

#[test]
fn an_ended_helper_refuses_follow_ups_and_leaves_both_mailboxes_unchanged() {
    let parent = mailbox(Admission::Free);
    let helper = helper();
    let address = helper.address();
    report(
        &address,
        &parent.address(),
        ChildStatus::Replied(FOValue::Unit),
    );
    let (_birth_report, _exchange) = take(&parent);
    address.end(CancelCause::Cancelled);
    assert!(matches!(
        address.post(message("too late", true)),
        Err(Refused::Ended)
    ));
    assert_eq!(
        address.status().report,
        Some(ChildStatus::Replied(FOValue::Unit))
    );
    assert!(!address.status().owes_parent);
    assert_eq!(depth(&parent, Source::Agent), 0);
}
