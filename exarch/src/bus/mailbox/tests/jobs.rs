use super::*;

#[test]
fn begin_records_one_job_before_its_exchange_records_and_settles_the_same_id() {
    let dir = OwnDir::for_test("mailbox-job-records").unwrap();
    let record = Recorder::create(&dir).unwrap();
    let mailbox = recorded_mailbox(Admission::Free, record.clone());
    let _first = start(&mailbox);
    let id = mailbox.address().status().job.unwrap();
    let _recorded = record
        .emit(Forensic::SystemNote {
            text: "first exchange".into(),
        })
        .unwrap();
    mailbox.address().steer("mid-job".into()).unwrap();
    let (_input, _second) = take(&mailbox);
    assert!(mailbox.begin());
    assert_eq!(mailbox.address().status().job, Some(id));
    settled(&mailbox, false);
    let records = read_records(&dir.path().join(RECORD)).unwrap();
    assert!(matches!(records.as_slice(), [
        Record::Forensic(Forensic::JobOpened),
        Record::Forensic(Forensic::SystemNote { text }),
        Record::Forensic(Forensic::JobSettled { job, stopped: false }),
    ] if text == "first exchange" && *job == id));
    let (_, mut commits) = record.reader().since(None).unwrap();
    let opened = commits.next().unwrap().unwrap();
    assert_eq!(id, JobId::from(opened.number()));
}

#[test]
fn stop_with_a_stale_id_leaves_the_job_exchange_and_queued_text_unchanged() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    let _first = start(&mailbox);
    let stale = address.status().job.unwrap();
    settled(&mailbox, false);
    let current = start(&mailbox);
    let id = address.status().job.unwrap();
    assert_ne!(id, stale);
    address.steer("keep".into()).unwrap();
    assert_eq!(address.stop(stale), None);
    assert!(!current.is_cancelled());
    assert_eq!(address.queued(), ["keep"]);
    assert_eq!(address.stop(id), Some(vec!["keep".into()]));
    settled(&mailbox, true);
}

#[test]
fn stop_between_a_mid_job_pop_and_exchange_execution_cancels_the_minted_exchange() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    let first = start(&mailbox);
    address.steer("next".into()).unwrap();
    let (input, exchange) = take(&mailbox);
    assert!(matches!(input, Input::Human(text) if text == "next"));
    let id = address.status().job.unwrap();
    assert_eq!(address.stop(id), Some(Vec::new()));
    assert_eq!(exchange.cause(), Some(CancelCause::Interrupted));
    assert!(!first.is_cancelled(), "Stop names the latest exchange only");
    assert!(mailbox.begin());
    assert_eq!(exchange.cause(), Some(CancelCause::Interrupted));
    settled(&mailbox, true);
}

#[test]
fn stop_at_rest_including_after_a_pop_before_begin_is_refused() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    assert!(!address.stop_current());
    address.steer("first".into()).unwrap();
    let (_input, exchange) = take(&mailbox);
    address.steer("queued".into()).unwrap();
    assert!(!address.stop_current());
    assert!(!exchange.is_cancelled());
    assert_eq!(address.queued(), ["queued"]);
    assert!(address.status().job.is_none());
}

#[test]
fn stop_recalls_only_human_text_and_preserves_helpers_and_the_job() {
    let mailbox = mailbox(Admission::Free);
    let exchange = start(&mailbox);
    let address = mailbox.address();
    let helper_life = mailbox.life().child();
    let helper_exchange = Lease::under(&helper_life);
    let job = address.status().job;
    let id = job.unwrap();
    address.steer("first".into()).unwrap();
    address.post(surface()).unwrap();
    address.steer("second".into()).unwrap();
    address.steer("third".into()).unwrap();
    address.post(message("peer", false)).unwrap();
    address.post(wakeup(1, "tick")).unwrap();
    assert_eq!(
        address.stop(id),
        Some(vec!["first".into(), "second\nthird".into()])
    );
    assert_eq!(exchange.cause(), Some(CancelCause::Interrupted));
    assert!(address.is_live());
    assert!(!helper_life.is_cancelled());
    assert!(!helper_exchange.is_cancelled());
    assert_eq!(address.status().job, job);
    let (_batch, _exchange) = take(&mailbox);
    assert!(matches!(
        mailbox.drain().as_slice(),
        [
            Drained::Input(Input::Message(_)),
            Drained::Input(Input::Wakeup { .. })
        ]
    ));
    address
        .post(child(ChildStatus::Replied(FOValue::Unit)))
        .unwrap();
    assert!(matches!(take(&mailbox).0, Input::Child { .. }));
    assert_eq!(address.status().job, job);
    settled(&mailbox, true);
}

#[test]
fn queued_input_after_stop_runs_a_live_exchange_but_settlement_stays_stopped() {
    let mailbox = helper();
    let first = start(&mailbox);
    let address = mailbox.address();
    let job = address.status().job;
    address.post(message("follow up", true)).unwrap();
    assert_eq!(address.stop(job.unwrap()), Some(Vec::new()));
    let (input, exchange) = take(&mailbox);
    assert!(matches!(input, Input::Message(message) if message.from_parent));
    assert!(first.is_cancelled());
    assert!(!exchange.is_cancelled());
    assert!(mailbox.begin());
    assert_eq!(address.status().job, job);
    settled(&mailbox, true);
}

#[test]
fn peer_message_racing_settlement_is_taken_in_the_job_or_refused_at_rest() {
    for _ in 0..32 {
        let mailbox = helper();
        let _exchange = start(&mailbox);
        let address = mailbox.address();
        let job = address.status().job;
        let barrier = Barrier::new(2);
        let (posted, next) = std::thread::scope(|scope| {
            let post = scope.spawn(|| {
                barrier.wait();
                address.post(message("peer", false))
            });
            barrier.wait();
            let next = mailbox.next(QUIET);
            (post.join().unwrap(), next)
        });
        match (posted, next) {
            (
                Ok(()),
                Next::Take {
                    input: Input::Message(message),
                    ..
                },
            ) => {
                assert_eq!(message.text, "peer");
                assert_eq!(address.status().job, job);
                settled(&mailbox, false);
            }
            (Err(Refused::Resting), Next::Settled { stopped: false }) => {
                assert!(address.status().job.is_none());
            }
            _ => panic!("a peer post and settlement must linearize under the same lock"),
        }
    }
}

#[test]
fn every_post_kind_racing_settlement_is_taken_in_the_job_or_opens_the_next() {
    type Post = fn(&AgentAddress);
    type Expect = fn(&Input) -> bool;
    let posts: [(Post, Expect); 3] = [
        (
            |address| address.steer("x".into()).unwrap(),
            |input| matches!(input, Input::Human(text) if text == "x"),
        ),
        (
            |address| address.post(wakeup(1, "tick")).unwrap(),
            |input| matches!(input, Input::Wakeup { .. }),
        ),
        (
            |address| address.post(surface()).unwrap(),
            |input| matches!(input, Input::Surface { .. }),
        ),
    ];
    for (post, expected) in posts {
        for _ in 0..32 {
            let mailbox = mailbox(Admission::Free);
            let _exchange = start(&mailbox);
            let address = mailbox.address();
            let job = address.status().job.unwrap();
            let barrier = Barrier::new(2);
            let next = std::thread::scope(|scope| {
                let poster = scope.spawn(|| {
                    barrier.wait();
                    post(&address);
                });
                barrier.wait();
                let next = mailbox.next(QUIET);
                poster.join().unwrap();
                next
            });
            match next {
                Next::Take { input, .. } => {
                    assert!(expected(&input));
                    assert_eq!(address.status().job, Some(job));
                    settled(&mailbox, false);
                }
                Next::Settled { stopped: false } => {
                    let (input, _exchange) = take(&mailbox);
                    assert!(expected(&input), "a post racing settlement is not lost");
                    assert!(mailbox.begin());
                    assert_ne!(address.status().job, Some(job));
                }
                _ => panic!("a post and settlement must linearize under the same lock"),
            }
        }
    }
}

#[test]
fn helper_and_parent_traffic_in_both_directions_never_deadlocks() {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..200 {
            let parent = mailbox(Admission::Free);
            let helper = helper();
            let (up, down) = (parent.address(), helper.address());
            let barrier = Barrier::new(4);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    down.report(
                        ChildStatus::Replied(FOValue::Unit),
                        Some((&up, child(ChildStatus::Replied(FOValue::Unit)))),
                    );
                });
                scope.spawn(|| {
                    barrier.wait();
                    down.end(CancelCause::Cancelled);
                    down.report(
                        ChildStatus::Failed("ended".into()),
                        Some((&up, child(ChildStatus::Stopped))),
                    );
                });
                scope.spawn(|| {
                    barrier.wait();
                    let _ = down.post(message("follow up", true));
                });
                scope.spawn(|| {
                    barrier.wait();
                    let _ = up.post(child(ChildStatus::Stopped));
                    let _ = up.status();
                });
            });
        }
        done.send(()).unwrap();
    });
    assert!(
        finished.recv_timeout(Duration::from_secs(30)).is_ok(),
        "report, end and post across a parent and helper lock must not deadlock"
    );
}

#[test]
fn stop_racing_settlement_has_the_same_verdict_as_the_settled_record() {
    for _ in 0..32 {
        let mailbox = mailbox(Admission::Free);
        let _exchange = start(&mailbox);
        let address = mailbox.address();
        let id = address.status().job.unwrap();
        let barrier = Barrier::new(2);
        let (stop, next) = std::thread::scope(|scope| {
            let stop = scope.spawn(|| {
                barrier.wait();
                address.stop(id)
            });
            barrier.wait();
            let next = mailbox.next(QUIET);
            (stop.join().unwrap(), next)
        });
        assert!(matches!(next, Next::Settled { stopped } if stopped == stop.is_some()));
        assert!(address.status().job.is_none());
        assert_eq!(address.stop(id), None);
    }
}

#[test]
fn a_failed_job_open_or_settlement_ends_the_bound_life() {
    for fail_on_open in [false, true] {
        let dir = OwnDir::for_test("mailbox-writer-failure").unwrap();
        let record = Recorder::create(&dir).unwrap();
        let mailbox = recorded_mailbox(Admission::Free, record.clone());
        if fail_on_open {
            record.fail_after(0);
            assert!(!mailbox.begin());
            assert!(mailbox.address().status().job.is_none());
        } else {
            let _exchange = start(&mailbox);
            record.fail_after(0);
        }
        assert!(matches!(mailbox.next(QUIET), Next::Ended));
        assert!(!mailbox.address().is_live());
        assert!(record.check_available().is_err());
        assert!(
            !read_records(&dir.path().join(RECORD))
                .unwrap()
                .iter()
                .any(|record| { matches!(record, Record::Forensic(Forensic::JobSettled { .. })) })
        );
    }
}
