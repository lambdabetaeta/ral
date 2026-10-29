use super::*;

fn model_pick() -> crate::provider::Pick {
    crate::provider::Pick::of(&crate::provider::Provider::scripted(
        "queued-model",
        crate::provider::scripted::Script::new(),
    ))
}

#[test]
fn adjacent_human_fragments_coalesce_without_crossing_other_inputs() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.steer("first α".into()).unwrap();
    address.steer(String::new()).unwrap();
    address.steer("second β".into()).unwrap();
    address.post(wakeup(1, "tick")).unwrap();
    address.steer("third".into()).unwrap();
    assert_eq!(address.queued(), ["first α\n\nsecond β", "third"]);
    assert_eq!(depth(&mailbox, Source::User), 2);
    assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "first α\n\nsecond β"));
    assert!(matches!(take(&mailbox).0, Input::Wakeup { text, .. } if text == "tick"));
    assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "third"));
    assert_eq!(depth(&mailbox, Source::User), 0);
}

#[test]
fn queued_shows_human_text_and_commands_in_order() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.post(wakeup(1, "tick")).unwrap();
    address.steer("first".into()).unwrap();
    address
        .command(Command::Read(Read::Hatch {
            memory: Memory::Mnemon,
            name: Some("scout".into()),
        }))
        .unwrap();
    address.steer("second".into()).unwrap();
    address.command(Command::Model(model_pick())).unwrap();
    address.command(Command::Rewrite(Rewrite::Evict)).unwrap();
    assert_eq!(
        address.queued(),
        [
            "first",
            "/branch scout",
            "second",
            "/model queued-model",
            "/evict"
        ]
    );
}

#[test]
fn recall_extracts_only_human_text_preserving_other_inputs() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.steer("first".into()).unwrap();
    address.post(wakeup(1, "tick")).unwrap();
    address.steer("second".into()).unwrap();
    address.steer("third".into()).unwrap();
    address.command(Command::Read(Read::Resources)).unwrap();
    address.post(message("peer", false)).unwrap();
    address.steer("fourth".into()).unwrap();
    assert_eq!(address.recall(), ["first", "second\nthird", "fourth"]);
    assert_eq!(address.queued(), ["/resources"]);
    let inputs = mailbox.drain();
    assert!(matches!(
        inputs.as_slice(),
        [
            Drained::Input(Input::Wakeup { .. }),
            Drained::Read(Read::Resources),
            Drained::Input(Input::Message(_))
        ]
    ));
    assert_eq!(address.recall(), [] as [String; 0]);
}

#[test]
fn drain_takes_messages_batches_reads_and_wakeups_until_the_first_rewrite() {
    let mailbox = mailbox(Admission::Free);
    let _exchange = start(&mailbox);
    let address = mailbox.address();
    address
        .post(message("please inspect the parser", false))
        .unwrap();
    address.post(surface()).unwrap();
    address.post(wakeup(1, "run the tests")).unwrap();
    address.command(Command::Read(Read::Resources)).unwrap();
    let pick = model_pick();
    address.command(Command::Model(pick.clone())).unwrap();
    address.steer("before".into()).unwrap();
    address
        .command(Command::Rewrite(Rewrite::Rewind(7)))
        .unwrap();
    address.steer("after".into()).unwrap();
    assert!(
        matches!(mailbox.drain().as_slice(), [Drained::Input(Input::Message(message)), Drained::Input(Input::Surface { generation: 4, .. }), Drained::Input(Input::Wakeup { epoch: 3, .. }), Drained::Read(Read::Resources), Drained::Model(model), Drained::Input(Input::Human(text))]
        if model == &pick && message.from == AgentId::new(7) && message.from_name == "review"
        && message.render() == "[EXARCH AGENT 7 MESSAGE: review]\nplease inspect the parser\n[/EXARCH]"
        && text == "before")
    );
    assert!(mailbox.drain().is_empty());
    assert!(matches!(
        mailbox.next(QUIET),
        Next::Command(Command::Rewrite(Rewrite::Rewind(7)))
    ));
    assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "after"));
}

#[test]
fn cancelled_exchange_drains_nothing_but_the_next_exchange_takes_queued_input() {
    let mailbox = mailbox(Admission::Free);
    let exchange = start(&mailbox);
    let address = mailbox.address();
    address.post(message("resume", false)).unwrap();
    address.post(surface()).unwrap();
    assert!(address.stop_current());
    assert_eq!(exchange.cause(), Some(CancelCause::Interrupted));
    assert!(mailbox.drain().is_empty());
    let (input, next) = take(&mailbox);
    assert!(matches!(input, Input::Message(message) if message.text == "resume"));
    assert!(!next.is_cancelled());
    assert!(matches!(
        mailbox.drain().as_slice(),
        [Drained::Input(Input::Surface { .. })]
    ));
}

#[test]
fn clear_drops_reports_gone_wakeups_and_batches_but_keeps_all_typed_input() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.steer("old".into()).unwrap();
    address.post(child(ChildStatus::Stopped)).unwrap();
    address.post(Input::Gone(AgentId::new(8))).unwrap();
    address.post(wakeup(1, "tick")).unwrap();
    address.post(surface()).unwrap();
    address.post(message("peer", false)).unwrap();
    address.command(Command::Rewrite(Rewrite::Clear)).unwrap();
    address.steer("new".into()).unwrap();
    mailbox.clear();
    assert!(
        matches!(mailbox.drain().as_slice(), [Drained::Input(Input::Human(text)), Drained::Input(Input::Message(message))]
        if text == "old" && message.text == "peer")
    );
    assert!(matches!(
        mailbox.next(QUIET),
        Next::Command(Command::Rewrite(Rewrite::Clear))
    ));
    assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "new"));
    assert!(mailbox.source_depths().iter().all(|(_, count)| *count == 0));
}

#[test]
fn source_depths_count_every_kind_without_changing_the_queue() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.steer("a".into()).unwrap();
    address.steer("b".into()).unwrap();
    address.post(wakeup(1, "tick")).unwrap();
    address.post(child(ChildStatus::Stopped)).unwrap();
    address.post(Input::Gone(AgentId::new(8))).unwrap();
    address.post(message("peer", false)).unwrap();
    address.post(surface()).unwrap();
    address.command(Command::Read(Read::Context)).unwrap();
    address.command(Command::Model(model_pick())).unwrap();
    address.command(Command::Rewrite(Rewrite::Evict)).unwrap();
    assert_eq!(
        mailbox.source_depths(),
        vec![
            (Source::User, 1),
            (Source::Schedule, 1),
            (Source::Agent, 2),
            (Source::Message, 1),
            (Source::Read, 1),
            (Source::Rewrite, 1),
            (Source::Surface, 1),
            (Source::Model, 1),
        ]
    );
    let snapshot = mailbox.source_depths();
    assert_eq!(snapshot, mailbox.source_depths());
    assert!(matches!(take(&mailbox).0, Input::Human(text) if text == "a\nb"));
}

#[test]
fn human_peer_and_child_inputs_count_as_prompting() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.post(surface()).unwrap();
    address.post(wakeup(1, "tick")).unwrap();
    assert!(!mailbox.has_input());
    address.steer("human".into()).unwrap();
    assert!(mailbox.has_input());
    assert_eq!(address.recall(), ["human"]);
    assert!(!mailbox.has_input());
    address.post(message("peer", false)).unwrap();
    assert!(mailbox.has_input());
    let _drained = mailbox.drain();
    assert!(!mailbox.has_input());
    address.post(child(ChildStatus::Stopped)).unwrap();
    assert!(mailbox.has_input());
}

#[test]
fn ending_refuses_posts_and_ignores_queued_work() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    address.steer("queued".into()).unwrap();
    address.end(CancelCause::Cancelled);
    assert!(matches!(mailbox.next(QUIET), Next::Ended));
    assert!(matches!(address.steer("late".into()), Err(Refused::Ended)));
    assert_eq!(address.queued(), [] as [String; 0]);
    assert_eq!(address.recall(), [] as [String; 0]);
    assert!(!address.release());
    assert!(!mailbox.begin());
    let before = mailbox.source_depths();
    assert_eq!(address.stop(1.into()), None);
    assert!(address.command(Command::Read(Read::Resources)).is_err());
    assert_eq!(mailbox.source_depths(), before);
}

#[test]
fn dropping_the_mailbox_ends_retained_addresses_and_their_exchange() {
    let mailbox = mailbox(Admission::Free);
    let address = mailbox.address();
    let weak = address.downgrade();
    let exchange = start(&mailbox);
    drop(mailbox);
    assert!(!address.is_live());
    assert!(exchange.is_cancelled());
    assert!(matches!(address.post(surface()), Err(Refused::Ended)));
    assert!(weak.upgrade().is_some());
    drop(address);
    assert!(weak.upgrade().is_none());
}

#[test]
fn a_parked_owner_observes_ancestor_cancellation() {
    let root = DurableRoot::default();
    let mailbox = AgentMailbox::new(
        root.child(),
        Recorder::none(),
        Admission::Free,
        crate::provider::Pick::of(&crate::provider::Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new(),
        )),
    );
    let address = mailbox.address();
    let worker = std::thread::spawn(move || mailbox.next(QUIET));
    root.cancel(CancelCause::Cancelled);
    let finished = wait_until(|| worker.is_finished());
    address.end(CancelCause::Cancelled);
    assert!(matches!(worker.join().unwrap(), Next::Ended));
    assert!(
        finished,
        "ancestor cancellation must wake a parked owner within {WAIT:?}"
    );
}
