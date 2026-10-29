use super::*;

#[test]
fn a_gated_root_holds_text_but_runs_commands_before_release() {
    let mailbox = mailbox(Admission::Gated);
    let address = mailbox.address();
    address.steer("held".into()).unwrap();
    address.command(Command::Read(Read::Resources)).unwrap();
    assert!(matches!(
        mailbox.next(QUIET),
        Next::Command(Command::Read(Read::Resources))
    ));
    assert_eq!(address.queued(), ["held"]);
    assert!(address.status().job.is_none());
    assert!(address.release());
    assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "held"));
    assert!(mailbox.begin());
    settled(&mailbox, false);
    address.steer("second job".into()).unwrap();
    let (send, recv) = mpsc::channel();
    let worker = std::thread::spawn(move || send.send(mailbox.next(QUIET)).unwrap());
    let parked = wait_until(|| address.status().waiting_for_input);
    let held = recv.try_recv();
    let released = address.release();
    let next = recv.recv_timeout(WAIT);
    address.end(CancelCause::Cancelled);
    worker.join().unwrap();
    assert!(parked, "the held owner must park awaiting a release");
    assert!(matches!(held, Err(mpsc::TryRecvError::Empty)));
    assert!(released);
    assert!(
        matches!(next.unwrap(), Next::Take { input: Input::Human(text), .. } if text == "second job")
    );
}

#[test]
fn release_cannot_revive_an_ended_gated_root() {
    let mailbox = mailbox(Admission::Gated);
    let address = mailbox.address();
    address.steer("held".into()).unwrap();
    address.end(CancelCause::Cancelled);

    assert!(!address.release());
    assert!(matches!(mailbox.next(QUIET), Next::Ended));
}

#[test]
fn quiet_inputs_bypass_every_admission_without_consuming_release_or_opening_jobs() {
    for admission in [
        Admission::Free,
        Admission::Gated,
        Admission::Parent { idle: WAIT },
    ] {
        let mailbox = mailbox(admission);
        let address = mailbox.address();
        address
            .post(wakeup(1, "held for parent admission"))
            .unwrap();
        address.post(child(ChildStatus::Stopped)).unwrap();
        address.post(Input::Gone(AgentId::new(8))).unwrap();
        address.command(Command::Rewrite(Rewrite::Evict)).unwrap();
        if admission == Admission::Free {
            assert!(matches!(take(&mailbox).0, Input::Wakeup { .. }));
        }
        assert!(matches!(take(&mailbox).0, Input::Child { .. }));
        assert!(matches!(take(&mailbox).0, Input::Gone(_)));
        assert!(matches!(
            mailbox.next(QUIET),
            Next::Command(Command::Rewrite(Rewrite::Evict))
        ));
        assert!(address.status().job.is_none());
        if admission != Admission::Free {
            assert_eq!(depth(&mailbox, Source::Schedule), 1);
        }
    }
}

#[test]
fn quiet_inputs_leave_an_existing_gate_release_for_the_next_accepted_prompt() {
    let mailbox = mailbox(Admission::Gated);
    let address = mailbox.address();
    assert!(address.release());
    address.post(child(ChildStatus::Stopped)).unwrap();
    assert!(matches!(take(&mailbox).0, Input::Child { .. }));
    address.steer("accepted".into()).unwrap();
    assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "accepted"));
    assert!(mailbox.begin());
    settled(&mailbox, false);
    assert!(matches!(
        mailbox.shared.lock().phase,
        Phase::Resting {
            released: false,
            ..
        }
    ));
}

#[test]
fn resting_helpers_refuse_peers_and_resume_on_their_parent_or_the_human() {
    for parent in [false, true] {
        let mailbox = helper();
        let address = mailbox.address();
        address.report(ChildStatus::Replied(FOValue::Unit), None);
        assert!(!address.status().working());
        assert!(matches!(
            address.post(message("peer", false)),
            Err(Refused::Resting)
        ));
        address.post(wakeup(1, "held")).unwrap();
        if parent {
            address.post(message("parent", true)).unwrap();
            assert!(address.status().owes_parent);
        } else {
            address.steer("human".into()).unwrap();
            assert!(!address.status().owes_parent);
        }
        let (input, _exchange) = take(&mailbox);
        assert!(matches!(
            (parent, input),
            (true, Input::Message(_)) | (false, Input::Human(_))
        ));
        assert!(address.status().job.is_none());
        assert!(mailbox.begin());
        assert!(address.status().working());
        assert!(address.post(message("peer while working", false)).is_ok());
        assert!(matches!(take(&mailbox).0, Input::Wakeup { .. }));
        assert!(matches!(take(&mailbox).0, Input::Message(message) if !message.from_parent));
        settled(&mailbox, false);
        assert!(matches!(
            address.post(message("resting peer", false)),
            Err(Refused::Resting)
        ));
    }
}

#[test]
fn a_resting_helper_times_out_and_refuses_later_input() {
    let idle = Duration::from_millis(25);
    let started = Instant::now();
    let mailbox = mailbox(Admission::Parent { idle });
    let address = mailbox.address();
    address.post(wakeup(1, "held")).unwrap();
    let worker = std::thread::spawn(move || mailbox.next(QUIET));
    let finished = wait_until(|| worker.is_finished());
    let cause = address.0.life.cause();
    address.end(CancelCause::Cancelled);
    assert!(matches!(worker.join().unwrap(), Next::Ended));
    assert!(finished, "a resting helper must enforce its idle deadline");
    assert!(started.elapsed() >= idle);
    assert_eq!(cause, Some(CancelCause::TimedOut));
    assert!(matches!(
        address.post(message("too late", true)),
        Err(Refused::Ended)
    ));
}

#[test]
fn idle_expiry_does_not_end_an_open_job_before_it_settles() {
    let mailbox = mailbox(Admission::Parent {
        idle: Duration::ZERO,
    });
    let _exchange = start(&mailbox);
    settled(&mailbox, false);
    assert!(mailbox.address().is_live());
    assert!(matches!(mailbox.next(QUIET), Next::Ended));
    assert_eq!(mailbox.life().cause(), Some(CancelCause::TimedOut));
}

#[test]
fn a_parent_message_racing_the_idle_limit_is_taken_or_refused_ended() {
    for _ in 0..32 {
        let mailbox = mailbox(Admission::Parent {
            idle: Duration::ZERO,
        });
        let address = mailbox.address();
        let barrier = Barrier::new(2);
        let (posted, next) = std::thread::scope(|scope| {
            let post = scope.spawn(|| {
                barrier.wait();
                address.post(message("at the deadline", true))
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
                    exchange,
                },
            ) => {
                assert_eq!(message.text, "at the deadline");
                assert!(!exchange.is_cancelled());
                assert!(mailbox.begin());
                assert!(address.status().job.is_some());
            }
            (Err(Refused::Ended), Next::Ended) => {
                assert_eq!(mailbox.life().cause(), Some(CancelCause::TimedOut));
            }
            _ => panic!("idle expiry and posting must linearize under the same lock"),
        }
    }
}

#[test]
fn waiting_for_input_and_busy_distinguish_human_wait_from_pending_helpers() {
    for rest in [
        QUIET,
        Rest {
            pending: true,
            schedules: false,
        },
        Rest {
            pending: false,
            schedules: true,
        },
    ] {
        let mailbox = mailbox(Admission::Free);
        let address = mailbox.address();
        let _exchange = start(&mailbox);
        let job = address.status().job;
        let (pending, schedules) = (rest.pending, rest.schedules);
        if !pending && !schedules {
            settled(&mailbox, false);
        }
        let worker = std::thread::spawn(move || mailbox.next(rest));
        let waiting = wait_until(|| {
            let status = address.status();
            status.busy == pending && status.waiting_for_input == (!pending && !schedules)
        });
        address.steer("resume".into()).unwrap();
        let finished = wait_until(|| worker.is_finished());
        address.end(CancelCause::Cancelled);
        let next = worker.join().unwrap();
        assert!(waiting);
        assert!(finished);
        assert!(matches!(next, Next::Take { input: Input::Human(text), .. } if text == "resume"));
        if pending || schedules {
            assert_eq!(address.status().job, job);
        }
    }
}
