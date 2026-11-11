//! Severance seen only by the probe after an exchange.

use super::*;

/// The exchange's own scope is detached, so the severance cancels nothing it
/// watches; the text-only reply touches no engine, so only the worker probe
/// after the exchange meets the loss.
#[test]
fn an_engine_lost_before_the_post_exchange_probe_fails_the_exchange_with_the_loss() {
    let mut session = Agent::for_test("system").unwrap();
    let (emit, _rx) = emitter(&session);
    set_provider(
        &mut session,
        scripted("test-model", Script::new().then(Reply::text("done"))),
    );
    session.handle().steer("go".into()).unwrap();
    let Next::Take { input, .. } = session.mailbox.next(session.rest()) else {
        panic!("a queued human input is taken up")
    };
    let prompt = session.admit(input).unwrap();
    assert!(session.mailbox.begin());
    let cause = Severed::Closed(ral_core::fact::ConnectionClosed::TransportDropped);
    session.engine().transport().sever(cause.clone());
    let outcome = session.exchange(&DurableRoot::default().child(), prompt, &emit);
    let AgentOutcome::Failed(reason) = outcome else {
        panic!("a severed engine fails the exchange");
    };
    assert_eq!(reason, crate::record::ChildFailure::severed(&cause));
}
