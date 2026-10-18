use super::*;
use crate::bus::AgentOutcome;
use crate::record::{AgentId, Display, Record};

/// Every arrival records its display commit: a prompt
/// commits `Display::Prompt`, and a subagent's breadcrumb commits
/// `Display::SubagentDone`.
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

    session.arrive(&Item::Human("hello".into()), Some(1));
    session.arrive(
        &Item::Agent(AgentResult {
            id: AgentId::new(7),
            name: "helper".into(),
            outcome: AgentOutcome::Failed("boom".into()),
            elapsed: std::time::Duration::from_millis(1500),
        }),
        None,
    );

    let mut facts: Vec<&'static str> = Vec::new();
    for record in crate::bus::drain_records(&rx) {
        match record {
            Record::Display(Display::Prompt { text, turn }) => {
                assert_eq!((text.as_str(), turn), ("hello", Some(1)));
                facts.push("prompt");
            }
            Record::Display(Display::SubagentDone {
                name,
                outcome,
                elapsed_ms,
                ..
            }) => {
                assert_eq!(name, "helper");
                assert_eq!(
                    outcome,
                    crate::record::Settlement::Failed {
                        reason: "boom".into()
                    }
                );
                assert_eq!(elapsed_ms, 1500);
                facts.push("subagent");
            }
            _ => {}
        }
    }
    assert_eq!(facts, ["prompt", "subagent"], "both commits land");
}
