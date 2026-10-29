use super::*;
use crate::record::{Memory, RECORD, Record, read_records};
use ral_core::own::OwnDir;
use ral_core::process::DurableRoot;
use ral_core::test_helper::eventually;
use std::sync::{Barrier, mpsc};

mod admission;
mod integration;
mod jobs;
mod queue;
mod reports;
mod wakeups;

const QUIET: Rest = Rest {
    pending: false,
    schedules: false,
};
const WAIT: Duration = Duration::from_secs(5);

fn mailbox(admission: Admission) -> AgentMailbox {
    recorded_mailbox(admission, Recorder::none())
}

fn recorded_mailbox(admission: Admission, record: Recorder) -> AgentMailbox {
    let life = DurableRoot::default().child();
    record.bind(life.clone());
    AgentMailbox::new(life, record, admission)
}

fn helper() -> AgentMailbox {
    mailbox(Admission::Parent {
        idle: Duration::from_secs(60),
    })
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

fn message(text: &str, from_parent: bool) -> Input {
    Input::Message(AgentMessage {
        from: AgentId::new(7),
        from_name: "review".into(),
        text: text.into(),
        from_parent,
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
        epoch: 3,
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
    let status = mailbox.address().status();
    assert!(status.job.is_none());
    assert!(status.rest.is_some());
}

fn wait_until(mut probe: impl FnMut() -> bool) -> bool {
    eventually(WAIT, || probe().then_some(())).is_some()
}
