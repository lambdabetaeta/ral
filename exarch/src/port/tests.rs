use super::*;
use crate::agent::fleet::Fleet;
use crate::agent::testkit::{self, TestAgentSpec, scripted, set_provider, test_agent};
use crate::bus::{Next, Rest};
use crate::provider::scripted::{Reply, Script};
use crate::record::{Control, Forensic, Record, Selected};
use std::time::Instant;

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn ended_agents_refuse_every_port_operation_even_while_their_names_remain() {
    let fleet = Fleet::for_test();
    let agent = test_agent(&fleet, TestAgentSpec::new("ended")).unwrap();
    let port = Port::new(fleet.registry.clone(), UsageMeter::default());
    let id = agent.id();
    port.submit(id, "queued before end".into()).unwrap();
    port.end(id).unwrap();
    assert!(!fleet.registry.is_empty());
    assert_eq!(port.submit(id, "late".into()), Err(Ended(id)));
    assert_eq!(port.queued(id), Err(Ended(id)));
    assert_eq!(port.recall(id), Err(Ended(id)));
    assert_eq!(port.stop(id, 1.into()), Err(Ended(id)));
    assert_eq!(
        port.command(id, Command::Read(Read::Context)),
        Err(Ended(id))
    );
    assert_eq!(port.end(id), Err(Ended(id)));
    assert!(port.row(id).is_none());
    assert!(!port.is_live(id));
    assert!(port.roster().is_empty());
}

#[test]
fn roster_orders_live_mailbox_rows_by_id_with_their_recorded_selection_and_waiting_state() {
    let fleet = Fleet::for_test();
    let first = test_agent(&fleet, TestAgentSpec::new("z-first")).unwrap();
    let second = test_agent(&fleet, TestAgentSpec::new("a-second")).unwrap();
    let ended = test_agent(&fleet, TestAgentSpec::new("ended")).unwrap();
    ended.end();
    let selected = Selected::for_test("published-model");
    second
        .mailbox
        .transact(|txn| txn.change_model(selected.clone()))
        .unwrap()
        .unwrap();
    let port = Port::new(fleet.registry.clone(), UsageMeter::default());
    port.submit(second.id(), "working".into()).unwrap();
    let Next::Take { exchange, .. } = second.mailbox.next(Rest { pending: false }) else {
        panic!("the second agent takes its queued prompt")
    };
    assert!(second.mailbox.begin());
    let rows = port.roster();
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [first.id(), second.id()]
    );
    assert!(rows[0].waiting_for_input);
    assert_eq!(rows[0].selected, first.status().unwrap().selected);
    assert!(!rows[1].waiting_for_input);
    assert_eq!(rows[1].selected, selected);
    assert!(port.row(ended.id()).is_none());
    drop(exchange);
}

#[test]
fn a_port_retained_across_host_end_keeps_final_run_usage_but_no_live_rows() {
    let mut root = testkit::trunk(false);
    set_provider(
        &mut root,
        scripted(
            "test-model",
            Script::new()
                .then(Reply::stalled("partial"))
                .then(Reply::text("recovered")),
        ),
    );
    let host = testkit::host(root);
    let id = host.children()[0];
    let port = host.port();
    port.submit(id, "go".into()).unwrap();
    let deadline = Instant::now() + WAIT;
    let mut expected = Usage::default();
    loop {
        let signal = host
            .feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the exchange settles after recovering from a stalled turn");
        if let Signal::Commit(_, commit) = signal {
            let mut settled = false;
            for record in commit.records() {
                match record.value() {
                    Record::Forensic(Forensic::UsageDelta { usage, .. }) => expected += *usage,
                    Record::Control(Control::JobSettled { .. }) => settled = true,
                    _ => {}
                }
            }
            if settled {
                break;
            }
        }
    }
    assert_eq!(
        expected.unreported, 1,
        "the run's usage is not an empty total"
    );
    assert_eq!(host.end(), [] as [String; 0]);
    assert!(port.roster().is_empty());
    assert!(port.row(id).is_none());
    let final_usage = port.usage();
    assert_eq!(
        serde_json::to_value(final_usage).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}
