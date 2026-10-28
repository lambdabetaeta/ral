//! The desk's wire arm: `` `start `` making an engine through a fake factory
//! and a re-exec'd `--engine` child standing in for the guest daemon's
//! spawn, the same vehicle the wire seat's own tests use, since a genuine
//! vsock factory only means anything inside a real guest.

use super::super::*;
use super::{message_req, spec, start};
use crate::agent::roster::summary;
use crate::agent::testkit::TestAgent;
use crate::bus::Inbox;
use crate::cancel::InterruptTarget;
use crate::enquiry::ForkClaim;
use crate::record::AgentId;
use crate::record::AgentLog;
use crate::test_helper::spawn_engine_on;
use ral_core::seed::{Sealed, seal};
use ral_core::shell::runtime::NurseryId;
use std::os::unix::net::UnixStream;
use std::sync::Mutex as StdMutex;

/// Each log takes the next session id, so two of them are never the same
/// session to the wire.
fn fresh_log() -> AgentLog {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    AgentLog::for_test(
        AgentId::new(n),
        "test",
        &crate::record::RecordedAccount::for_test("test"),
    )
    .expect("session log")
}

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

/// A wire reach with a genuine `ControlSender` behind it — a disposable
/// `--engine` child adopted and killed at once, since these fixtures need
/// a real reach value's *shape* but never actually cancel or interrupt
/// through it.
fn fake_wire_reach() -> InterruptTarget {
    let (host, guest) = UnixStream::pair().expect("socketpair standing in for the factory");
    let mut child = spawn_engine_on(&guest);
    drop(guest);
    let transport = ral_core::carrier::WireTransport::adopt(
        host,
        ral_core::protocol::channel::Liveness::default(),
    )
    .expect("adopt host stream");
    let control = ral_core::carrier::Transport::control(&transport).clone();
    let _ = child.kill();
    let _ = child.wait();
    InterruptTarget::new(control)
}

/// A wire-seat desk fixture whose parent holds the very inbox this
/// returns, exactly [`super::spawnable_desk`]'s identity shape with
/// `kind: SeatKind::Wire` and a factory installed.
fn wire_spawnable_desk(
    fuel: u32,
    factory: Arc<FakeFactory>,
) -> (ExarchDesk, Arc<Fleet>, Inbox, TestAgent) {
    let parent_inbox = Inbox::new();
    let fleet = Fleet::new(
        crate::agent::fleet::Launch {
            factory: Some(factory),
            ..crate::agent::fleet::Launch::for_test()
        },
        crate::agent::fleet::AGENT_LEASE_IDLE,
    );
    let mut spec = crate::agent::testkit::TestAgentSpec::new("parent");
    spec.reach = fake_wire_reach();
    spec.mailbox = parent_inbox.mailbox();
    spec.fuel = fuel;
    spec.role = crate::agent::Role::Headless;
    let agent =
        crate::agent::testkit::test_agent(&fleet, spec).expect("a fresh fleet's wire trunk");
    let (emit, _rx) = crate::bus::dummy_emitter();
    let desk = ExarchDesk {
        services: HostServices {
            fleet: fleet.clone(),
            kind: SeatKind::Wire {
                engine: crate::boot::test_attach().0,
            },
            stamp: agent.mailbox.stamp(),
            agent: agent.clone(),
            emit,
            reply: ReplyCell::default(),
            log: LogCell::new(fresh_log()),
            acts: ActFragment::default(),
        },
    };
    (desk, fleet, parent_inbox, agent)
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
    let (desk, _fleet, _parent_inbox, _agent) = wire_spawnable_desk(3, factory);

    let err = desk
        .ask(wire_start_req("unattached", parent_seal()))
        .expect_err("an engine that never attached must be refused");
    assert!(
        err.message.contains("could not be started"),
        "got: {}",
        err.message
    );
    assert_eq!(
        summary(&desk.services.agent).live,
        0,
        "an engine that never attached names no child on the roster"
    );
}

/// A factory that cannot make an engine is refused with a sentence naming
/// the factory's reason.
#[test]
fn wire_spawn_refuses_naming_the_factory_when_it_refuses() {
    let factory = FakeFactory::new(Guest::Refuses("connection reset by peer".to_string()));
    let (desk, _fleet, _parent_inbox, _agent) = wire_spawnable_desk(3, factory);

    let err = desk
        .ask(wire_start_req("never-made", parent_seal()))
        .expect_err("a refused factory must be refused");
    assert!(
        err.message.contains("could not make a helper engine")
            && err.message.contains("connection reset by peer"),
        "must name the factory's refusal and its reason, got: {}",
        err.message
    );
}

/// The identity tag on a wire desk is a guest claiming to be in process.
/// Refused, and before the factory is asked.
#[test]
fn wire_spawn_refuses_a_parked_fork_without_asking_the_factory() {
    let factory = FakeFactory::new(Guest::Spawns);
    let (desk, _fleet, _parent_inbox, _agent) = wire_spawnable_desk(3, factory.clone());

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
}

/// The seal crosses the host untouched: the engine the factory made is a
/// child of the sealing parent, attached before the desk answers.  The seat is
/// taken straight off the desk, where it can be probed.
#[test]
fn a_sealed_bequest_reaches_the_child_engine() {
    let factory = FakeFactory::new(Guest::Spawns);
    let (desk, _fleet, _parent_inbox, _agent) = wire_spawnable_desk(3, factory.clone());
    let SeatKind::Wire { engine } = &desk.services.kind else {
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
    drop(child);

    desk.ask(wire_start_req("sealed-child", parent_seal()))
        .expect("a sealed start makes and rosters a child engine");
    assert_eq!(factory.calls(), 2);
    factory.reap();
}

/// The peer-messaging pin: one identity-reach and one wire-reach child of
/// the same parent exchange marked notes through the desk's `message`
/// handler, which touches only the tree and a mailbox — never a seat — so
/// sender and recipient never learn each other's transport.
#[test]
fn identity_and_wire_peers_exchange_messages_through_one_desk() {
    let fleet = Fleet::for_test();
    let mut parent_spec = crate::agent::testkit::TestAgentSpec::new("parent");
    parent_spec.fuel = 3;
    parent_spec.role = crate::agent::Role::Headless;
    let parent =
        crate::agent::testkit::test_agent(&fleet, parent_spec).expect("a fresh fleet's trunk");

    let identity_inbox = Inbox::new();
    let mut identity = crate::agent::testkit::TestAgentSpec::new("identity-peer");
    identity.role = crate::agent::Role::child(&parent);
    identity.mailbox = identity_inbox.mailbox();
    let _identity_peer = crate::agent::testkit::test_agent(&fleet, identity)
        .expect("a fresh child of a live parent");

    let wire_inbox = Inbox::new();
    let mut wire = crate::agent::testkit::TestAgentSpec::new("wire-peer");
    wire.role = crate::agent::Role::child(&parent);
    wire.reach = fake_wire_reach();
    wire.mailbox = wire_inbox.mailbox();
    let _wire_peer =
        crate::agent::testkit::test_agent(&fleet, wire).expect("a fresh child of a live parent");

    let (emit, _rx) = crate::bus::dummy_emitter();
    let desk = ExarchDesk {
        services: HostServices {
            fleet,
            // Neither peer's transport matters to `message`, which
            // touches only the tree and a mailbox.
            kind: SeatKind::Wire {
                engine: crate::boot::test_attach().0,
            },
            stamp: parent.mailbox.stamp(),
            agent: parent.clone(),
            emit,
            reply: ReplyCell::default(),
            log: LogCell::new(fresh_log()),
            acts: ActFragment::default(),
        },
    };

    desk.ask(message_req("identity-peer", "note for identity"))
        .expect("the parent may message its identity-reach descendant");
    desk.ask(message_req("wire-peer", "note for wire"))
        .expect("the parent may message its wire-reach descendant");

    match identity_inbox.next_item() {
        Some(crate::bus::Next::Item(crate::bus::Item::Message(m))) => {
            assert_eq!(m.text, "note for identity");
        }
        other => panic!("expected an AgentMessage item, got {other:?}"),
    }
    match wire_inbox.next_item() {
        Some(crate::bus::Next::Item(crate::bus::Item::Message(m))) => {
            assert_eq!(m.text, "note for wire");
        }
        other => panic!("expected an AgentMessage item, got {other:?}"),
    }
}
