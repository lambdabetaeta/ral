use super::*;
use crate::agent::Trunk;
use crate::agent::fleet::Fleet;
use crate::agent::testkit::{TestAgent, TestAgentSpec, test_agent, trunk};
use crate::bus::{Drained, Input, Next, Rest};
use ral_core::data::FOValue;

const REST: Rest = Rest {
    owing: false,
    lives: Vec::new(),
    schedules: false,
};

fn root(fleet: &Arc<Fleet>, name: &str) -> TestAgent {
    test_agent(fleet, TestAgentSpec::new(name)).unwrap()
}

fn child(fleet: &Arc<Fleet>, parent: &TestAgent, name: &str) -> TestAgent {
    test_agent(
        fleet,
        TestAgentSpec::child(name, parent, parent.mailbox.life().child()),
    )
    .unwrap()
}

fn family() -> (Arc<Fleet>, TestAgent, TestAgent) {
    let fleet = Fleet::for_test();
    let parent = root(&fleet, "parent");
    let helper = child(&fleet, &parent, "helper");
    (fleet, parent, helper)
}

fn empty(owner: &TestAgent) -> bool {
    owner
        .mailbox
        .source_depths()
        .iter()
        .all(|(_, depth)| *depth == 0)
}

fn take(owner: &TestAgent) -> Input {
    let Next::Take { input, .. } = owner.mailbox.next(&REST) else {
        panic!("expected a queued input")
    };
    input
}

fn report(parent: &TestAgent, helper: &TestAgent, status: &ChildStatus) {
    helper.report(status.clone());
    let Input::Child {
        id, status: actual, ..
    } = take(parent)
    else {
        panic!("a helper report")
    };
    assert_eq!(id, helper.id());
    assert_eq!(&actual, status);
}

#[test]
fn a_message_posts_parent_provenance_to_a_descendant() {
    let (_fleet, parent, helper) = family();
    parent
        .descendant(&helper)
        .unwrap()
        .message(&parent, "check the lexer".into())
        .unwrap();
    let Input::Message(message) = take(&helper) else {
        panic!("a message")
    };
    assert_eq!(message.from, parent.id());
    assert_eq!(message.from_name, "parent");
    assert_eq!(message.text, "check the lexer");
    assert!(message.from_parent);
    assert!(helper.status().owes_parent);
}

#[test]
fn the_scope_climb_refuses_self_and_non_descendants() {
    let (fleet, parent, branch) = family();
    let sibling = child(&fleet, &parent, "sibling");
    let grandchild = child(&fleet, &branch, "grandchild");
    for target in [&branch, &sibling, &parent] {
        assert!(branch.descendant(target).is_none());
    }
    assert_eq!(
        parent.descendant(&grandchild).unwrap().id(),
        grandchild.id()
    );
    assert_eq!(grandchild.root().id(), parent.id());
    assert_eq!(parent.root().id(), parent.id());
    assert!(parent.is_root());
    assert!(!grandchild.is_root());
}

#[test]
fn an_accepted_message_survives_its_senders_end() {
    let (fleet, parent, sender) = family();
    let receiver = root(&fleet, "receiver");
    receiver.message(&sender, "already said".into()).unwrap();
    parent.end();
    assert!(!sender.is_live());
    assert!(matches!(take(&receiver), Input::Message(message)
        if message.text == "already said" && !message.from_parent));
}

#[test]
fn one_report_answers_every_parent_message_taken_up() {
    let (_fleet, parent, helper) = family();
    for text in ["one", "two"] {
        helper.message(&parent, text.into()).unwrap();
        assert!(matches!(take(&helper), Input::Message(_)));
    }
    report(&parent, &helper, &ChildStatus::Replied(FOValue::Unit));
    assert!(!helper.status().working());
    assert!(empty(&parent));
}

#[test]
fn owner_drop_ends_the_tree_and_releases_names_without_handles_retaining_it() {
    let parent = trunk(Trunk::Converses);
    let child = parent.fork_named("child").unwrap();
    let grandchild = child.fork_named("grandchild").unwrap();
    let fleet = parent.fleet.clone();
    let handle = parent.handle();
    drop(parent);
    assert!(!handle.is_live());
    assert!(!child.handle().is_live());
    assert!(!grandchild.handle().is_live());
    assert!(fleet.name("main").is_ok());
    drop((child, grandchild));
    assert!(fleet.is_empty());
}

#[test]
fn owner_drop_reports_failure_for_a_queued_parent_follow_up() {
    let parent = trunk(Trunk::Converses);
    let child = parent.fork_named("helper").unwrap();
    child
        .handle()
        .message(&parent.handle(), "follow up".into())
        .unwrap();
    drop(child);
    let inputs = parent.mailbox.drain();
    assert!(
        matches!(&inputs[..], [Drained::Input(Input::Child { status: ChildStatus::Failed(reason), .. })]
        if reason == crate::bus::ENDED_BEFORE_REPORTING)
    );
}

#[test]
fn human_taken_up_before_stop_is_recorded_once_without_a_provider_turn() {
    use crate::agent::outcome::Outcome;
    use crate::record::{Forensic, Protocol, Record, Voice};
    let mut owner = trunk(Trunk::Converses);
    owner.handle().steer("taken up".into()).unwrap();
    let Next::Take { input, exchange } = owner.mailbox.next(&REST) else {
        panic!("human exchange")
    };
    let prompt = owner.admit(input).unwrap();
    assert!(owner.mailbox.begin());
    assert_eq!(
        owner.handle().stop(owner.handle().status().job.unwrap()),
        Some(Vec::new())
    );
    let (emit, rx) = crate::bus::dummy_emitter();
    let provider = owner.handle().current_provider();
    let outcome = owner
        .deliberate(
            &exchange,
            &provider,
            Some((prompt.text, prompt.voice)),
            prompt.continues,
            &emit,
        )
        .unwrap();
    assert!(matches!(outcome, Outcome::Cancelled));
    let records = crate::bus::drain_records(&rx);
    assert_eq!(records.iter().filter(|record| matches!(record,
        Record::Protocol(Protocol::UserPrompt { text, voice: Voice::Human, .. }) if text == "taken up")).count(), 1);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::TurnStarted { .. })))
    );
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::Cancelled)))
    );
    assert!(owner.is_ready());
}

#[test]
fn a_claimed_surface_declines_without_cancelling_the_current_exchange() {
    let mut owner = trunk(Trunk::Converses);
    owner.handle().steer("opening".into()).unwrap();
    let Next::Take { exchange, .. } = owner.mailbox.next(&REST) else {
        panic!("an exchange")
    };
    assert!(owner.mailbox.begin());
    let (emit, _rx) = crate::bus::dummy_emitter();
    let result = owner.ral(
        &exchange,
        "let h = spawn \"claimed\" { 7 }\nawait $h",
        10,
        &emit,
    );
    assert!(!result.failed, "{}", result.text);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let surface = loop {
        if let Some(input) = owner
            .mailbox
            .drain()
            .into_iter()
            .find_map(|drained| match drained {
                Drained::Input(input @ Input::Surface { .. }) => Some(input),
                _ => None,
            })
        {
            break input;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the claimed worker never surfaced"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(owner.admit(surface).is_none());
    assert!(!exchange.is_cancelled());
    assert!(owner.handle().status().job.is_some());
}
