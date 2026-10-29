use super::*;

#[test]
fn post_wakeup_with_observes_the_queue_under_the_post_lock() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    let id = ScheduleId::new(1);
    address
        .post_wakeup_with(id, |queued| {
            assert!(!queued);
            Some((3, "first".into()))
        })
        .unwrap();
    address
        .post_wakeup_with(id, |queued| {
            assert!(queued);
            None
        })
        .unwrap();
    address
        .post_wakeup_with(ScheduleId::new(2), |queued| {
            assert!(!queued);
            Some((4, "other".into()))
        })
        .unwrap();
    assert_eq!(depth(&mailbox, Source::Schedule), 2);
    assert!(
        matches!(take(&mailbox).0, Input::Wakeup { id: received, epoch: 3, text }
        if received == id && text == "first")
    );
    address
        .post_wakeup_with(id, |queued| {
            assert!(!queued);
            None
        })
        .unwrap();
    mailbox.clear();
    address
        .post_wakeup_with(ScheduleId::new(2), |queued| {
            assert!(!queued);
            None
        })
        .unwrap();
    assert_eq!(depth(&mailbox, Source::Schedule), 0);
}

#[test]
fn concurrent_wakeup_preparations_enqueue_only_one_delivery_for_the_same_id() {
    for _ in 0..32 {
        let mailbox = mailbox(Admission::Free);
        let address = mailbox.address();
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let post = || {
                barrier.wait();
                address
                    .post_wakeup_with(ScheduleId::new(1), |queued| {
                        (!queued).then(|| (3, "one delivery".into()))
                    })
                    .unwrap();
            };
            let first = scope.spawn(post);
            post();
            first.join().unwrap();
        });
        assert_eq!(depth(&mailbox, Source::Schedule), 1);
        assert!(matches!(take(&mailbox).0, Input::Wakeup { text, .. } if text == "one delivery"));
        assert_eq!(depth(&mailbox, Source::Schedule), 0);
    }
}

#[test]
fn clear_cannot_slip_between_wakeup_preparation_and_publication() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    let (prepared, preparing) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    let (clearing, clear_started) = mpsc::channel();
    std::thread::scope(|scope| {
        let post = scope.spawn(move || {
            address
                .post_wakeup_with(ScheduleId::new(1), |queued| {
                    assert!(!queued);
                    prepared.send(()).unwrap();
                    resumed.recv_timeout(WAIT).unwrap();
                    Some((3, "racing clear".into()))
                })
                .unwrap();
        });
        preparing.recv_timeout(WAIT).unwrap();
        let clear = scope.spawn(|| {
            clearing.send(()).unwrap();
            mailbox.clear();
        });
        clear_started.recv_timeout(WAIT).unwrap();
        resume.send(()).unwrap();
        post.join().unwrap();
        clear.join().unwrap();
    });
    assert_eq!(depth(&mailbox, Source::Schedule), 0);
}

#[test]
fn an_ended_mailbox_does_not_prepare_a_wakeup() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.end(CancelCause::Cancelled);
    let mut prepared = false;
    let posted = address.post_wakeup_with(ScheduleId::new(1), |_| {
        prepared = true;
        Some((3, "late".into()))
    });
    assert!(matches!(posted, Err(Refused::Ended)));
    assert!(!prepared);
}
