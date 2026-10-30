use super::*;
use crate::record::{Control, Forensic, Memory, RECORD, Record, Started, read_records};
use ral_core::own::OwnDir;
use ral_core::process::DurableRoot;
use ral_core::test_helper::eventually;
use std::sync::{Barrier, mpsc};

mod acceptance;
mod admission;
mod integration;
mod jobs;
mod queue;
mod reports;
mod wakeups;

const QUIET: Rest = Rest { pending: false };
const WAIT: Duration = Duration::from_secs(5);

fn mailbox(admission: Admission) -> AgentMailbox {
    recorded_mailbox(admission, Recorder::none())
}

fn recorded_mailbox(admission: Admission, record: Recorder) -> AgentMailbox {
    configured_mailbox(
        admission,
        record,
        DurableRoot::default().child(),
        None,
        matches!(admission, Admission::Parent { .. }),
    )
}

fn configured_mailbox(
    admission: Admission,
    record: Recorder,
    life: ScopeNode,
    parent: Option<AgentAddress>,
    returns: bool,
) -> AgentMailbox {
    record.bind(life.clone());
    let helper = matches!(admission, Admission::Parent { .. });
    let id = AgentId::new(if helper { 8 } else { 7 });
    let folds = record
        .begin(Started {
            session_id: id,
            name: if helper { "helper" } else { "parent" }.into(),
            parent: helper.then_some(AgentId::new(7)),
            returns,
            selected: Selected::for_test("test-model"),
            log_dir: std::path::PathBuf::default(),
            at_unix_ms: 0,
        })
        .unwrap();
    AgentMailbox::new(id, life, record, admission, parent, folds)
}

fn helper() -> AgentMailbox {
    mailbox(Admission::Parent {
        idle: Duration::from_secs(60),
    })
}

fn helper_of(parent: &AgentMailbox) -> AgentMailbox {
    configured_mailbox(
        Admission::Parent {
            idle: Duration::from_secs(60),
        },
        Recorder::none(),
        parent.life().child(),
        Some(parent.address()),
        true,
    )
}

fn publish(address: &AgentAddress, status: ChildStatus) {
    address
        .transact(|txn| txn.report(status, child))
        .unwrap()
        .unwrap();
}

fn take(mailbox: &AgentMailbox) -> (Input, Lease) {
    match mailbox.next(QUIET) {
        Next::Take { input, exchange } => (input, exchange),
        _ => panic!("expected an input and its exchange"),
    }
}

fn start(mailbox: &AgentMailbox) -> Lease {
    mailbox.address().steer("start".into()).unwrap();
    let (input, exchange) = take(mailbox);
    assert!(matches!(input, Input::Human(text) if text == "start"));
    assert!(mailbox.begin());
    exchange
}

fn task(text: &str) -> Input {
    Input::Task(AgentMessage {
        from: AgentId::new(7),
        from_name: "parent".into(),
        text: text.into(),
    })
}

fn message(text: &str, parent: bool) -> Input {
    Input::Message(AgentMessage {
        from: AgentId::new(if parent { 7 } else { 9 }),
        from_name: "review".into(),
        text: text.into(),
    })
}

fn child(status: ChildStatus) -> Input {
    Input::Child {
        id: AgentId::new(8),
        name: "helper".into(),
        status,
        elapsed: Duration::from_secs(2),
    }
}

fn wakeup(id: u64, text: &str) -> Input {
    Input::Wakeup {
        id: ScheduleId::new(id),
        text: text.into(),
    }
}

fn surface() -> Input {
    Input::Surface {
        generation: 4,
        values: vec![FOValue::Unit],
    }
}

fn depth(mailbox: &AgentMailbox, source: Source) -> u64 {
    mailbox
        .source_depths()
        .into_iter()
        .find(|(kind, _)| *kind == source)
        .expect("every source has a depth")
        .1
}

fn settled(mailbox: &AgentMailbox, stopped: bool) {
    assert!(matches!(mailbox.next(QUIET), Next::Settled { stopped: mark } if mark == stopped));
    let status = mailbox.address().status().unwrap();
    assert!(status.job.is_none());
    assert!(status.rest.is_some());
}

fn wait_until(mut probe: impl FnMut() -> bool) -> bool {
    eventually(WAIT, || probe().then_some(())).is_some()
}
