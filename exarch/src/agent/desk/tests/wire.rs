//! The desk's wire arm: `` `start `` making an engine through a fake factory
//! and a re-exec'd `--engine` child standing in for the guest daemon's
//! spawn, the same vehicle the wire engine's own tests use, since a genuine
//! vsock factory only means anything inside a real guest.

use super::*;
use crate::agent::testkit::{TestAgentSpec, test_agent};
use crate::test_helper::spawn_engine_on;
use ral_core::seed::{Sealed, seal};
use std::os::unix::net::UnixStream;
use std::sync::Mutex as StdMutex;

/// What the far end of a [`FakeFactory`] does when asked for an engine.
enum Guest {
    /// A real `--engine` child on the other end of a socketpair, as the
    /// guest daemon spawns one onto a connection.
    Spawns,
    /// A guest whose engine died at birth: a connection, and nobody on it.
    Closes,
    /// No engine could be made.
    Refuses(String),
}

/// A fake [`crate::agent::Factory`] that plays the guest daemon, so the desk
/// under test drives a real handshake against a real peer.
struct FakeFactory {
    guest: Guest,
    calls: StdMutex<usize>,
    /// Engine children the factory spawned, for the test to reap.
    children: StdMutex<Vec<std::process::Child>>,
}

impl FakeFactory {
    fn new(guest: Guest) -> Arc<Self> {
        Arc::new(Self {
            guest,
            calls: StdMutex::new(0),
            children: StdMutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }

    fn reap(&self) {
        for mut child in self.children.lock().unwrap().drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl crate::agent::Factory for FakeFactory {
    fn engine(&self) -> Result<ral_core::protocol::channel::WireStream, String> {
        *self.calls.lock().unwrap() += 1;
        if let Guest::Refuses(reason) = &self.guest {
            return Err(reason.clone());
        }
        let (host, guest) =
            UnixStream::pair().map_err(|e| format!("socketpair for the fake factory: {e}"))?;
        if matches!(self.guest, Guest::Spawns) {
            self.children.lock().unwrap().push(spawn_engine_on(&guest));
        }
        Ok(host)
    }
}

fn with_wire_desk<R>(
    factory: Arc<FakeFactory>,
    f: impl FnOnce(ExarchDesk, Arc<Fleet>, TestAgent) -> R,
) -> R {
    with_fleet_desk(
        3,
        crate::agent::fleet::Launch {
            factory: Some(factory),
            ..crate::agent::fleet::Launch::for_test()
        },
        |mut desk, fleet, agent| {
            desk.call.kind = EngineKind::Wire {
                engine: crate::boot::test_attach().0,
            };
            f(desk, fleet, agent)
        },
    )
}

/// The parent's bequest as a wire engine seals it, carrying `PARENT_MARK=1`.
fn parent_seal() -> Sealed {
    let mut shell = crate::boot::test_shell();
    shell.set_env_var("PARENT_MARK", "1");
    seal(&shell.bequeath(ral_core::SpawnGrant::Inherit)).expect("a bequest seals")
}

/// `` `exarch-agents `start `` as a wire engine sends it: its bequest, sealed.
fn wire_start_req(name: &str, seed: Sealed) -> Request {
    start(ForkClaim::Sealed(seed), spec("go", name))
}

/// An engine that died at birth leaves the host a connection and no child,
/// and it must say so rather than register one.
#[test]
fn wire_spawn_refuses_when_the_engine_closes_before_attaching() {
    let factory = FakeFactory::new(Guest::Closes);
    with_wire_desk(factory, |desk, _fleet, _agent| {
        let err = desk
            .ask(wire_start_req("unattached", parent_seal()))
            .expect_err("an engine that never attached must be refused");
        assert!(
            err.message.contains("could not be started"),
            "got: {}",
            err.message
        );
        assert_eq!(
            summary(&desk.call.handle, &desk.call.fleet).live,
            0,
            "an engine that never attached names no child on the roster"
        );
    });
}

/// A factory that cannot make an engine is refused with a sentence naming
/// the factory's reason.
#[test]
fn wire_spawn_refuses_naming_the_factory_when_it_refuses() {
    let factory = FakeFactory::new(Guest::Refuses("connection reset by peer".to_string()));
    with_wire_desk(factory, |desk, _fleet, _agent| {
        let err = desk
            .ask(wire_start_req("never-made", parent_seal()))
            .expect_err("a refused factory must be refused");
        assert!(
            err.message.contains("could not make a helper engine")
                && err.message.contains("connection reset by peer"),
            "must name the factory's refusal and its reason, got: {}",
            err.message
        );
    });
}

/// The identity tag on a wire desk is a guest claiming to be in process.
/// Refused, and before the factory is asked.
#[test]
fn wire_spawn_refuses_a_parked_fork_without_asking_the_factory() {
    let factory = FakeFactory::new(Guest::Spawns);
    with_wire_desk(factory.clone(), |desk, _fleet, _agent| {
        let err = desk
            .ask(super::start_req(NurseryId(0), "go", "in-process"))
            .expect_err("a wire desk has no nursery to adopt a parked fork from");
        assert!(
            err.message.contains("`sealed <bytes>`"),
            "must name the tag this host does take, got: {}",
            err.message
        );
        assert_eq!(
            factory.calls(),
            0,
            "a fork this desk cannot reach is refused before the factory is asked"
        );
    });
}

/// The seal crosses the host untouched: the engine the factory made is a
/// child of the sealing parent, attached before the desk answers. The engine is
/// taken straight off the desk, where it can be probed.
#[test]
fn a_sealed_bequest_reaches_the_child_engine() {
    let factory = FakeFactory::new(Guest::Spawns);
    with_wire_desk(factory.clone(), |desk, _fleet, _agent| {
        let EngineKind::Wire { engine } = &desk.call.kind else {
            panic!("a wire desk makes its children over a wire")
        };
        let child = desk
            .make_engine(engine, parent_seal())
            .expect("a sealed bequest makes a child engine");
        assert_eq!(
            child
                .read(|t| t.env_var("PARENT_MARK"))
                .expect("the child engine answers a probe"),
            Some("1".to_string()),
            "the parent's env var reached the child through the sealed bequest"
        );
        child.end().expect("the probed engine ends cleanly");
        drop(child);

        desk.ask(wire_start_req("sealed-child", parent_seal()))
            .expect("a sealed start makes and rosters a child engine");
        assert_eq!(factory.calls(), 2);
        desk.agent_dismiss("sealed-child")
            .expect("join before reaping the factory engines");
        factory.reap();
    });
}

#[test]
fn identity_and_wire_peers_exchange_messages_through_one_desk() {
    with_wire_desk(FakeFactory::new(Guest::Spawns), |desk, fleet, parent| {
        let identity_peer = test_agent(
            &fleet,
            TestAgentSpec::child("identity-peer", &parent, parent.mailbox.life().child()),
        )
        .unwrap();
        let wire_peer = test_agent(
            &fleet,
            TestAgentSpec::child("wire-peer", &parent, parent.mailbox.life().child()),
        )
        .unwrap();
        desk.ask(message_req("identity-peer", "note for identity"))
            .unwrap();
        desk.ask(message_req("wire-peer", "note for wire")).unwrap();
        for (peer, expected) in [
            (&identity_peer, "note for identity"),
            (&wire_peer, "note for wire"),
        ] {
            assert!(
                matches!(next_input(&peer.mailbox), Input::Message(message) if message.text == expected)
            );
        }
    });
}
