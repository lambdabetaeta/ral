//! The desk's wire arm: `` `start `` making an engine through a fake factory
//! and a re-exec'd `--engine` child standing in for the guest daemon's
//! spawn, the same vehicle the wire engine's own tests use, since a genuine
//! vsock factory only means anything inside a real guest.

use super::*;
use crate::test_helper::spawn_engine_on;
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
    Refuses(ral_core::fact::EngineFactoryFailure),
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
    fn engine(
        &self,
    ) -> Result<ral_core::protocol::channel::WireStream, ral_core::fact::EngineFactoryFailure> {
        *self.calls.lock().unwrap() += 1;
        if let Guest::Refuses(reason) = &self.guest {
            return Err(reason.clone());
        }
        let (host, guest) = UnixStream::pair()
            .map_err(|e| ral_core::fact::EngineFactoryFailure::Io(ral_core::io::failure(&e)))?;
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

/// The marshalled parent, carrying `PARENT_MARK=1`.
fn parent_bytes() -> Marshalled {
    let mut shell = crate::boot::test_shell();
    shell.set_env_var("PARENT_MARK", "1").unwrap();
    shell
        .marshal(&ral_core::SpawnGrant::Inherit)
        .expect("a test parent marshals")
}

/// An engine that died at birth leaves the host a connection and no child,
/// and it must say so rather than register one.
#[test]
fn wire_spawn_refuses_when_the_engine_closes_before_attaching() {
    let factory = FakeFactory::new(Guest::Closes);
    with_wire_desk(factory, |desk, _fleet, _agent| {
        let err = desk
            .ask(start_req(parent_bytes(), "go", "unattached"))
            .expect_err("an engine that never attached must be refused");
        assert!(
            matches!(&err.reason, Reason::Host(host) if matches!(&**host,
            HostFailure::Exarch(ExarchFailure::Engine(ral_core::fact::EngineFailure { step: ral_core::fact::EngineStep::Attach, cause: ral_core::fact::EngineCause::Connection(_) }))))
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
    let factory = FakeFactory::new(Guest::Refuses(ral_core::fact::EngineFactoryFailure::Io(
        ral_core::io::failure(&std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
    )));
    with_wire_desk(factory, |desk, _fleet, _agent| {
        let err = desk
            .ask(start_req(parent_bytes(), "go", "never-made"))
            .expect_err("a refused factory must be refused");
        assert_eq!(
            err.reason,
            Reason::Host(Box::new(HostFailure::Exarch(ExarchFailure::Engine(
                ral_core::fact::EngineFailure {
                    step: ral_core::fact::EngineStep::Factory,
                    cause: ral_core::fact::EngineCause::Factory(
                        ral_core::fact::EngineFactoryFailure::Io(ral_core::io::failure(
                            &std::io::Error::from(std::io::ErrorKind::ConnectionReset)
                        ))
                    ),
                }
            ))))
        );
    });
}

/// The parent crosses the host unread; the child attaches before the desk answers.
#[test]
fn a_marshalled_parent_reaches_the_child_engine() {
    let factory = FakeFactory::new(Guest::Spawns);
    with_wire_desk(factory.clone(), |desk, _fleet, _agent| {
        let EngineKind::Wire { engine } = &desk.call.kind else {
            panic!("a wire desk makes its children over a wire")
        };
        let parent = parent_bytes();
        let child = desk
            .make_engine(engine, parent.clone())
            .expect("a marshalled parent makes a child engine");
        assert_eq!(
            child
                .read(
                    ral_core::fact::ProbeSubject::EnvVar("PARENT_MARK".into()),
                    |t| t.env_var("PARENT_MARK")
                )
                .expect("the child engine answers a probe"),
            Some("1".to_string()),
            "the parent's env var reached the child through marshalling"
        );
        child.end().expect("the probed engine ends cleanly");
        drop(child);

        desk.ask(start_req(parent, "go", "marshalled-child"))
            .expect("a marshalled start makes and rosters a child engine");
        assert_eq!(factory.calls(), 2);
        desk.agent_dismiss("marshalled-child")
            .expect("join before reaping the factory engines");
        factory.reap();
    });
}
