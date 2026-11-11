use super::{Fleet, Unborn};
use crate::agent::testkit::{TestAgent, TestAgentSpec, forked_child, test_agent};
use crate::agent::{Agent, TestLaunch};
use crate::bus::{Next, Rest};
use crate::record::ChildStatus;
use ral_core::data::FOValue;
use ral_core::process::CancelCause;
use std::sync::Arc;
use std::time::Duration;

mod directory;
mod roots;

fn agent(fleet: &Arc<Fleet>, name: &str, parent: Option<&TestAgent>) -> TestAgent {
    let spec = parent.map_or_else(
        || TestAgentSpec::new(name),
        |parent| TestAgentSpec::child(name, parent, parent.mailbox.home().child()),
    );
    test_agent(fleet, spec).expect("a fresh directory name")
}

#[test]
fn configured_helper_idle_reaches_the_child_mailbox() {
    let parent = Agent::for_test_with(TestLaunch {
        helper_idle: Duration::ZERO,
        ..TestLaunch::new("system")
    })
    .unwrap();
    assert_eq!(parent.fleet.launch.idle, Duration::ZERO);
    let helper = forked_child(&parent, "short-lived", "`inherit");
    let rest = Rest { pending: false };
    helper.handle().steer("task".into()).unwrap();
    let Next::Take { exchange, .. } = helper.mailbox.next(rest) else {
        panic!("admitted input takes precedence over the idle limit");
    };
    assert!(helper.mailbox.begin());
    assert!(!exchange.is_cancelled());
    helper
        .handle()
        .report(ChildStatus::Replied(FOValue::Unit))
        .unwrap();
    assert!(matches!(helper.mailbox.next(rest), Next::Settled { .. }));
    assert!(
        helper.handle().is_live(),
        "settlement does not end the life"
    );
    assert!(matches!(helper.mailbox.next(rest), Next::Ended));
    assert_eq!(helper.mailbox.home().cause(), Some(CancelCause::TimedOut));
    assert!(parent.handle().is_live());
    assert!(parent.fleet.registry.resolve("short-lived").is_none());
    assert!(matches!(
        parent.fleet.registry.name("short-lived"),
        Err(Unborn::NameTaken(_))
    ));
    drop(helper);
    assert!(parent.fleet.registry.name("short-lived").is_ok());
}

#[test]
fn unborn_projects_the_rejected_name_without_rendering_it() {
    use ral_core::fact::{AgentFailure, AgentFailureNameTaken, AgentNameFailure};
    let fleet = Fleet::for_test();
    let malformed = fleet.registry.name("not a name").err().unwrap();
    assert_eq!(
        malformed.failure(),
        AgentFailure::NameMalformed(AgentNameFailure {
            name: "not a name".into()
        })
    );
    let _reserved = fleet.registry.name("reserved").unwrap();
    let taken = fleet.registry.name("reserved").err().unwrap();
    assert_eq!(
        taken.failure(),
        AgentFailure::NameTaken(AgentFailureNameTaken {
            name: "reserved".into()
        })
    );
}
