use super::*;
use crate::bus::AgentOutcome;
use crate::record::{AgentId, Display, Record};

/// An arrival records only what no protocol record speaks for: a subagent's
/// breadcrumb commits `Display::SubagentDone`, and a prompt commits nothing,
/// since its `Protocol` record is the one fact of it.
#[test]
fn arrival_records_display_facts() {
    use crate::bus::AgentResult;

    let session = Avatar::for_test("system").unwrap();
    let (tx, rx) = crate::bus::channel();
    session.recorder().attach(Box::new(crate::bus::FleetSink {
        id: AgentId::new(0),
        tx: tx.downgrade(),
        meter: crate::bus::UsageMeter::default(),
    }));

    session.arrive(&Item::Human("hello".into()));
    session.arrive(&Item::Agent(AgentResult {
        id: AgentId::new(7),
        name: "helper".into(),
        outcome: AgentOutcome::Failed("boom".into()),
        elapsed: std::time::Duration::from_millis(1500),
    }));

    let mut facts = crate::bus::drain_records(&rx)
        .into_iter()
        .filter_map(|record| match record {
            Record::Display(display) => Some(display),
            Record::Protocol(_) | Record::Forensic(_) => None,
        });
    let Some(Display::SubagentDone {
        name,
        outcome,
        elapsed_ms,
    }) = facts.next()
    else {
        panic!("the subagent's breadcrumb is the first display commit");
    };
    assert_eq!(name, "helper");
    assert_eq!(
        outcome,
        crate::record::Settlement::Failed {
            reason: "boom".into()
        }
    );
    assert_eq!(elapsed_ms, 1500);
    assert!(facts.next().is_none(), "the prompt commits no display fact");
}
