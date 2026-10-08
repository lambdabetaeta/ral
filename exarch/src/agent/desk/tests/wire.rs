//! The desk's wire arm: `` `start `` dialling a guest that is listening for
//! exactly one connection, over a fake dialler and a re-exec'd `--engine`
//! child standing in for the hatched guest — the same vehicle the wire seat's
//! own tests already use, since a genuine vsock dial only means anything
//! inside a real guest.

use super::super::*;
use super::{confined, message_req, spec, start};
use crate::agent::roster::summary;
use crate::bus::Inbox;
use crate::cancel::InterruptTarget;
use crate::enquiry::ForkClaim;
use crate::record::AgentId;
use crate::record::AgentLog;
use ral_core::types::NurseryId;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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

/// Re-exec this binary as a bare `--engine` child holding `guest` on fd 3,
/// exactly [`crate::agent::seat::tests::spawn_engine`]'s vehicle. The
/// caller drops its own copy afterwards, as `hatch_over` does, so the
/// child's death reads as EOF on the host's end.
fn spawn_engine_on(guest: &UnixStream) -> std::process::Child {
    let guest_fd = guest.as_raw_fd();
    let mut cmd = std::process::Command::new(std::env::current_exe().expect("current exe"));
    cmd.arg(ral_core::Role::Engine.flag());
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    // SAFETY: runs between fork and exec, calling only async-signal-safe
    // `dup2`/`close`, with no allocation and no locking.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if libc::dup2(guest_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if guest_fd != 3 {
                libc::close(guest_fd);
            }
            Ok(())
        });
    }
    cmd.spawn().expect("spawn engine child")
}

/// What the guest at the far end of a [`FakeDial`] does with the dial.
enum Guest {
    /// The whole spine: read the token, spawn the child, then ack — the
    /// order the guest's hatch in `ral_core::hatch` keeps, since the ack is the
    /// claim that the child exists.
    Hatches,
    /// A guest-side hatch that failed: read the token and close, saying
    /// nothing. The host's ack read sees EOF.
    ClosesWithoutAcking,
    /// Nothing is listening on that port.
    Refuses(String),
}

/// A fake [`crate::agent::Dial`] that plays the guest. `dial` hands back
/// one end of a socketpair and leaves a thread on the other end doing
/// whatever this fake's [`Guest`] says, so the desk under test drives a
/// real handshake against a real peer.
struct FakeDial {
    guest: Guest,
    /// Every port `dial` was asked for, in order.
    ports: StdMutex<Vec<u32>>,
    /// Every token a guest thread actually read off the wire.
    tokens: Arc<StdMutex<Vec<u64>>>,
    /// Engine children the guest threads spawned, for the test to reap.
    children: Arc<StdMutex<Vec<std::process::Child>>>,
}

impl FakeDial {
    fn new(guest: Guest) -> Arc<Self> {
        Arc::new(Self {
            guest,
            ports: StdMutex::new(Vec::new()),
            tokens: Arc::new(StdMutex::new(Vec::new())),
            children: Arc::new(StdMutex::new(Vec::new())),
        })
    }

    fn ports(&self) -> Vec<u32> {
        self.ports.lock().unwrap().clone()
    }
}

impl crate::agent::Dial for FakeDial {
    fn dial(&self, port: u32) -> Result<ral_core::protocol::channel::WireStream, String> {
        self.ports.lock().unwrap().push(port);
        let hatches = match &self.guest {
            Guest::Refuses(reason) => return Err(reason.clone()),
            Guest::Hatches => true,
            Guest::ClosesWithoutAcking => false,
        };
        let (host, guest) =
            UnixStream::pair().map_err(|e| format!("socketpair for the fake dial: {e}"))?;
        let tokens = self.tokens.clone();
        let children = self.children.clone();
        std::thread::spawn(move || {
            let mut guest = guest;
            let mut claim = [0u8; 8];
            if guest.read_exact(&mut claim).is_err() {
                return;
            }
            tokens.lock().unwrap().push(u64::from_le_bytes(claim));
            if !hatches {
                return;
            }
            let child = spawn_engine_on(&guest);
            guest
                .write_all(&[ral_core::protocol::HATCH_ACK])
                .expect("ack the hatch");
            children.lock().unwrap().push(child);
        });
        Ok(host)
    }
}

/// A wire reach with a genuine `ControlSender` behind it — a disposable
/// `--engine` child adopted and killed at once, since these fixtures need
/// a real reach value's *shape* but never actually cancel or interrupt
/// through it.
fn fake_wire_reach() -> InterruptTarget {
    let (host, guest) = UnixStream::pair().expect("socketpair standing in for the dial");
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
/// `kind: SeatKind::Wire` and a dialler installed.
fn wire_spawnable_desk(fuel: u32, dial: Arc<FakeDial>) -> (ExarchDesk, Arc<Fleet>, Inbox) {
    let parent_inbox = Inbox::new();
    let fleet = Fleet::new(
        crate::agent::fleet::Launch {
            dial: Some(dial),
            ..crate::agent::fleet::Launch::for_test()
        },
        crate::agent::fleet::AGENT_LEASE_IDLE,
    );
    let mut spec = crate::agent::testkit::TestAgentSpec::new("parent");
    spec.reach = fake_wire_reach();
    spec.mailbox = parent_inbox.mailbox();
    spec.fuel = fuel;
    spec.returns = true;
    spec.search = true;
    let agent =
        crate::agent::testkit::test_agent(&fleet, spec).expect("a fresh fleet's wire trunk");
    let (emit, _rx) = crate::bus::dummy_emitter();
    let desk = ExarchDesk {
        services: HostServices {
            fleet: fleet.clone(),
            kind: SeatKind::Wire {
                cwd: PathBuf::from("/work"),
                home: PathBuf::from("/tmp"),
            },
            stamp: agent.mailbox.stamp(),
            agent,
            emit,
            reply: ReplyCell::default(),
            log: LogCell::new(fresh_log()),
            branch: None,
            acts: ActFragment::default(),
            principal: ral_core::host::user(),
        },
    };
    (desk, fleet, parent_inbox)
}

/// `` `exarch-agents `start `` as a listening engine sends it: the port its
/// listener is bound to, and the token that listener will check the
/// host's dial against.
fn wire_start_req(name: &str, port: u32, token: u64) -> Request {
    start(
        ForkClaim::Listening { port, token },
        spec("go", name, confined(), false),
    )
}

/// A guest whose own hatch failed closes without acking. The host has a
/// connection and no child, and must say so rather than register one.
#[test]
fn wire_spawn_refuses_when_the_guest_closes_without_acking() {
    let dial = FakeDial::new(Guest::ClosesWithoutAcking);
    let (desk, _fleet, _parent_inbox) = wire_spawnable_desk(3, dial);

    let err = desk
        .ask(wire_start_req("unacked", 41_731, 7))
        .expect_err("an unacknowledged hatch must be refused");
    assert!(
        err.message
            .contains("the guest closed the connection before acknowledging the hatch"),
        "got: {}",
        err.message
    );
    assert_eq!(
        summary(&desk.services.agent).live,
        0,
        "a hatch that was never acknowledged names no child on the roster"
    );
}

/// A dial the guest refuses — nothing bound on that port — is refused
/// with a sentence naming the dial, so the builtin's own listener thread
/// learns why it was never reached.
#[test]
fn wire_spawn_refuses_naming_the_dial_when_the_guest_refuses_it() {
    let dial = FakeDial::new(Guest::Refuses("connection reset by peer".to_string()));
    let (desk, _fleet, _parent_inbox) = wire_spawnable_desk(3, dial);

    let err = desk
        .ask(wire_start_req("never-reached", 41_731, 7))
        .expect_err("a refused dial must be refused");
    assert!(
        err.message.contains("could not dial") && err.message.contains("41731"),
        "must name the dial and the port, got: {}",
        err.message
    );
}

/// The identity tag on a wire desk is a guest claiming to be in process.
/// Refused, and before anything is dialled.
#[test]
fn wire_spawn_refuses_a_parked_fork_without_dialling() {
    let dial = FakeDial::new(Guest::Hatches);
    let (desk, _fleet, _parent_inbox) = wire_spawnable_desk(3, dial.clone());

    let err = desk
        .ask(super::start_req(
            NurseryId(0),
            "go",
            "in-process",
            false,
        ))
        .expect_err("a wire desk has no nursery to adopt a parked fork from");
    assert!(
        err.message.contains("`listening [port, token]`"),
        "must name the tag this host does take, got: {}",
        err.message
    );
    assert!(
        dial.ports().is_empty(),
        "a fork this desk cannot reach is refused before any dial"
    );
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
    parent_spec.returns = true;
    parent_spec.search = true;
    let parent =
        crate::agent::testkit::test_agent(&fleet, parent_spec).expect("a fresh fleet's trunk");

    let identity_inbox = Inbox::new();
    let mut identity = crate::agent::testkit::TestAgentSpec::new("identity-peer");
    identity.parent = Some(parent.clone());
    identity.mailbox = identity_inbox.mailbox();
    let _identity_peer = crate::agent::testkit::test_agent(&fleet, identity)
        .expect("a fresh child of a live parent");

    let wire_inbox = Inbox::new();
    let mut wire = crate::agent::testkit::TestAgentSpec::new("wire-peer");
    wire.parent = Some(parent.clone());
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
                cwd: PathBuf::from("/"),
                home: PathBuf::from("/tmp"),
            },
            stamp: parent.mailbox.stamp(),
            agent: parent,
            emit,
            reply: ReplyCell::default(),
            log: LogCell::new(fresh_log()),
            branch: None,
            acts: ActFragment::default(),
            principal: ral_core::host::user(),
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
