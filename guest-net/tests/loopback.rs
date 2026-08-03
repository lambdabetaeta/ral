#![allow(clippy::disallowed_methods)]

//! A second `smoltcp` stack playing the guest, talking to `guest_net::run`'s
//! host stack over a `UnixStream` pair — the same frame codec and
//! [`device::Wire`] abstraction both ends of the real link use. `GuestSide`
//! drives its own `Interface`/`SocketSet` by hand, exactly the shape a real
//! guest kernel's TCP/IP stack would exercise.
//!
//! Every test supplies its own [`TestDialer`], the crate's one injectable
//! resolve-and-dial seam, so nothing here ever opens a socket to a real
//! host: an "allowed" CONNECT is vetted exactly as production code vets it
//! (`resolve` answers whatever address the case chose, `vet::open`'s own
//! `is_public` filter runs for real), but `dial` is free to ignore that
//! address and connect to a local fixture instead.
//!
//! Unix-only: the harness needs a `UnixStream` pair to stand in for the net
//! wire.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};

use exarch::egress::Egress;
use exarch::net_policy::Host;

use guest_net::{Config, vet};

/// The interface's own address — `vm_manager::GUEST_LINK.gateway` in the
/// real deployment, where the guest's `HTTPS_PROXY` also points.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// The guest's own address on the link — `vm_manager::GUEST_LINK.address`.
const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const PORT: u16 = 3128;
const SOCKET_BUF: usize = 8 * 1024;
/// Matches `stack::TUNNEL_CAP`. Not importable (private, deliberately —
/// see that module's doc), so restated here as the number this test's
/// capacity case must exercise.
const TUNNEL_CAP: usize = 32;

/// A minimal guest: a `smoltcp` interface driven by hand over one end of a
/// `UnixStream` pair, the other end handed to [`guest_net::run`].
struct GuestSide {
    device: guest_net::device::TunDevice<UnixStream>,
    iface: Interface,
    sockets: SocketSet<'static>,
}

impl GuestSide {
    fn new(guest_end: UnixStream) -> Self {
        let (mut device, _stop) = guest_net::device::spawn(guest_end, None).expect("reader starts");
        let mut config = IfaceConfig::new(HardwareAddress::Ip);
        config.random_seed = 1;
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(GUEST_ADDR), 24))
                .unwrap();
        });
        Self {
            device,
            iface,
            sockets: SocketSet::new(Vec::new()),
        }
    }

    fn poll(&mut self) {
        self.iface
            .poll(Instant::now(), &mut self.device, &mut self.sockets);
    }

    /// Poll until `done` is satisfied or `timeout` elapses. Generous
    /// margins throughout this file: this host's own scheduling jitter is
    /// high, and every wait here is only a handful of round trips over a
    /// loopback socket pair.
    fn poll_until(&mut self, timeout: Duration, mut done: impl FnMut(&mut Self) -> bool) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.poll();
            if done(self) {
                return true;
            }
            if std::time::Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn tcp_connect(&mut self, addr: Ipv4Addr, port: u16, local_port: u16) -> SocketHandle {
        let socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF]),
            tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF]),
        );
        let handle = self.sockets.add(socket);
        let ctx = self.iface.context();
        self.sockets
            .get_mut::<tcp::Socket>(handle)
            .connect(ctx, (IpAddress::Ipv4(addr), port), local_port)
            .expect("a fresh socket can always start connecting");
        handle
    }

    fn tcp_state(&self, handle: SocketHandle) -> tcp::State {
        self.sockets.get::<tcp::Socket>(handle).state()
    }

    fn tcp_send(&mut self, handle: SocketHandle, data: &[u8]) {
        let socket = self.sockets.get_mut::<tcp::Socket>(handle);
        let n = socket
            .send_slice(data)
            .expect("a fresh socket has room for this test's payload");
        assert_eq!(n, data.len(), "test payload must fit in one send");
    }

    fn tcp_can_recv(&self, handle: SocketHandle) -> bool {
        self.sockets.get::<tcp::Socket>(handle).can_recv()
    }

    fn tcp_recv(&mut self, handle: SocketHandle) -> Vec<u8> {
        let socket = self.sockets.get_mut::<tcp::Socket>(handle);
        let mut buf = [0u8; 4096];
        let n = socket.recv_slice(&mut buf).unwrap_or(0);
        buf[..n].to_vec()
    }

    fn tcp_close(&mut self, handle: SocketHandle) {
        self.sockets.get_mut::<tcp::Socket>(handle).close();
    }

    /// The host closed its half: a FIN arrived. This, not [`Self::tcp_done`],
    /// is what a refusal produces — the guest is still free to send.
    fn tcp_peer_closed(&self, handle: SocketHandle) -> bool {
        matches!(
            self.tcp_state(handle),
            tcp::State::CloseWait
                | tcp::State::LastAck
                | tcp::State::Closing
                | tcp::State::Closed
                | tcp::State::TimeWait
        )
    }

    /// Both halves are done. `TimeWait` counts: a guest that closes first
    /// sits there for 2MSL, which no test can afford to wait out.
    fn tcp_done(&self, handle: SocketHandle) -> bool {
        matches!(
            self.tcp_state(handle),
            tcp::State::Closed | tcp::State::TimeWait
        )
    }
}

/// The one injected [`vet::Dialer`]. `resolve` answers `resolve_to` — a
/// public-looking address by default, so `vet::open`'s own classifier runs
/// honestly; `dial` ignores that address and connects to `fixture` — a
/// loopback listener this test owns — after sleeping `dial_delay`, and
/// panics instead of resolving if `panic_on_resolve` is set.
struct TestDialer {
    fixture: SocketAddr,
    resolve_to: SocketAddr,
    dial_delay: Duration,
    panic_on_resolve: bool,
    resolve_calls: AtomicUsize,
    dial_calls: AtomicUsize,
}

/// Nothing listens at the default `fixture`: a case that dials at all when
/// it should not fails loudly rather than quietly tunnelling.
impl Default for TestDialer {
    fn default() -> Self {
        Self {
            fixture: "127.0.0.1:1".parse().unwrap(),
            resolve_to: "93.184.216.34:443".parse().unwrap(),
            dial_delay: Duration::ZERO,
            panic_on_resolve: false,
            resolve_calls: AtomicUsize::new(0),
            dial_calls: AtomicUsize::new(0),
        }
    }
}

impl vet::Dialer for TestDialer {
    fn resolve(&self, _host: &Host) -> std::io::Result<Vec<SocketAddr>> {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_on_resolve,
            "injected worker panic for the panic-reporting test"
        );
        Ok(vec![self.resolve_to])
    }

    fn dial(&self, _addr: SocketAddr, timeout: Duration) -> std::io::Result<TcpStream> {
        self.dial_calls.fetch_add(1, Ordering::SeqCst);
        if !self.dial_delay.is_zero() {
            thread::sleep(self.dial_delay);
        }
        TcpStream::connect_timeout(&self.fixture, timeout)
    }
}

/// A loopback listener that echoes whatever it reads back until EOF.
fn spawn_echo_fixture() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// A loopback listener that writes continuously and never reads, to fill
/// the tunnel's worker→guest ring on purpose.
fn spawn_flood_fixture() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let chunk = vec![0u8; 8192];
            while stream.write_all(&chunk).is_ok() {}
        }
    });
    addr
}

fn connect_request(host: &str) -> Vec<u8> {
    format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\nUser-Agent: test\r\n\r\n")
        .into_bytes()
}

fn harness(dialer: Arc<dyn vet::Dialer>) -> (GuestSide, guest_net::Session<UnixStream>) {
    harness_with(dialer, Egress::for_test())
}

/// [`harness`] over a caller-built [`Egress`] — the one case that has to
/// know where the ledger lands.
fn harness_with(
    dialer: Arc<dyn vet::Dialer>,
    egress: Egress,
) -> (GuestSide, guest_net::Session<UnixStream>) {
    let (host_end, guest_end) = UnixStream::pair().expect("a socket pair");
    let session = guest_net::run(
        host_end,
        Config {
            egress,
            gateway: GATEWAY,
            dialer,
        },
    )
    .expect("the host stack starts");
    (GuestSide::new(guest_end), session)
}

/// Runs `session.join()` on its own thread and waits at most `timeout` for
/// it — a hung join must fail this test loudly rather than hang the whole
/// suite.
fn join_within(
    session: guest_net::Session<UnixStream>,
    timeout: Duration,
) -> Option<Result<(), String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(session.join());
    });
    rx.recv_timeout(timeout).ok()
}

/// The host stack holds one address, the gateway, and does not use
/// `set_any_ip`. A SYN for anything else is not addressed to it at all, so
/// it is dropped rather than reset: the connection goes nowhere.
#[test]
fn direct_traffic_to_an_arbitrary_address_never_reaches_a_worker() {
    let dialer = Arc::new(TestDialer::default());
    let (mut guest, session) = harness(dialer.clone());
    let handle = guest.tcp_connect(Ipv4Addr::new(93, 184, 216, 34), 443, 40000);
    let established = guest.poll_until(Duration::from_secs(2), |g| {
        g.tcp_state(handle) == tcp::State::Established
    });
    assert!(!established, "a direct address must never establish");
    assert_eq!(dialer.resolve_calls.load(Ordering::SeqCst), 0);
    assert_eq!(dialer.dial_calls.load(Ordering::SeqCst), 0);
    session.stop();
    join_within(session, Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

#[test]
fn a_port_other_than_3128_has_no_listener() {
    let (mut guest, session) = harness(Arc::new(TestDialer::default()));
    let handle = guest.tcp_connect(GATEWAY, 9999, 40001);
    let closed = guest.poll_until(Duration::from_secs(5), |g| {
        matches!(
            g.tcp_state(handle),
            tcp::State::Closed | tcp::State::TimeWait
        )
    });
    assert!(closed, "a port with no listener must reset, not hang");
    session.stop();
    join_within(session, Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

#[test]
fn a_denied_connect_never_dials() {
    let dialer = Arc::new(TestDialer::default());
    let (mut guest, session) = harness(dialer.clone());
    let handle = guest.tcp_connect(GATEWAY, PORT, 40002);
    let established = guest.poll_until(Duration::from_secs(5), |g| {
        g.tcp_state(handle) == tcp::State::Established
    });
    assert!(established, "the proxy port must accept the handshake");
    guest.tcp_send(handle, &connect_request("off-list.example"));
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle)));
    let refused = String::from_utf8(guest.tcp_recv(handle)).expect("the status line is UTF-8");
    assert!(refused.starts_with("HTTP/1.1 403"), "got: {refused}");
    assert!(
        refused.contains(&exarch::net_policy::refusal("off-list.example")),
        "the guest must be handed the policy's own wording, not a second copy: {refused}"
    );
    let closed = guest.poll_until(Duration::from_secs(5), |g| g.tcp_peer_closed(handle));
    assert!(closed, "a denied host must close the connection");
    assert_eq!(
        dialer.resolve_calls.load(Ordering::SeqCst),
        0,
        "a denied host must never be resolved"
    );
    assert_eq!(
        dialer.dial_calls.load(Ordering::SeqCst),
        0,
        "a denied host must never dial"
    );
    session.stop();
    join_within(session, Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

/// The authority-confusion string that bypassed the old intercepting
/// proxy: `decode` must refuse it before policy or DNS is consulted.
#[test]
fn the_authority_confusion_regression_is_refused_before_any_dial() {
    let dialer = Arc::new(TestDialer::default());
    let (mut guest, session) = harness(dialer.clone());
    let handle = guest.tcp_connect(GATEWAY, PORT, 40003);
    let established = guest.poll_until(Duration::from_secs(5), |g| {
        g.tcp_state(handle) == tcp::State::Established
    });
    assert!(established);
    guest.tcp_send(
        handle,
        b"CONNECT archive.ubuntu.com@169.254.169.254:443 HTTP/1.1\r\nHost: archive.ubuntu.com@169.254.169.254:443\r\n\r\n",
    );
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle)));
    assert!(guest.tcp_recv(handle).starts_with(b"HTTP/1.1 403"));
    let closed = guest.poll_until(Duration::from_secs(5), |g| g.tcp_peer_closed(handle));
    assert!(closed, "the userinfo-confusion target must be refused");
    assert_eq!(dialer.resolve_calls.load(Ordering::SeqCst), 0);
    assert_eq!(dialer.dial_calls.load(Ordering::SeqCst), 0);
    session.stop();
    join_within(session, Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

/// DNS rebinding, end to end: a host the IT department really did approve,
/// whose answer is the cloud metadata address. Policy admits the name, so
/// `vet::open`'s own `is_public` filter is all that stands between the
/// guest and the host's own network — and its refusal must reach the guest
/// instead of a tunnel.
#[test]
fn an_allowed_host_that_resolves_to_a_link_local_address_never_dials() {
    let dialer = Arc::new(TestDialer {
        // A dial that did happen would succeed, and say so loudly.
        fixture: spawn_echo_fixture(),
        resolve_to: "169.254.169.254:443".parse().unwrap(),
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer.clone());
    let handle = guest.tcp_connect(GATEWAY, PORT, 40010);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(handle)
            == tcp::State::Established)
    );
    guest.tcp_send(handle, &connect_request("a.example"));

    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle)));
    let refused = String::from_utf8(guest.tcp_recv(handle)).expect("the status line is UTF-8");
    assert!(refused.starts_with("HTTP/1.1 403"), "got: {refused}");
    assert!(refused.contains("no public address"), "got: {refused}");
    assert_eq!(
        dialer.resolve_calls.load(Ordering::SeqCst),
        1,
        "policy admitted the name, so it really was resolved"
    );
    assert_eq!(
        dialer.dial_calls.load(Ordering::SeqCst),
        0,
        "a non-public answer must never be dialled"
    );
    let closed = guest.poll_until(Duration::from_secs(5), |g| g.tcp_peer_closed(handle));
    assert!(closed, "a refused host must close the connection");
    session.stop();
    join_within(session, Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

/// An allowed CONNECT reaches the fixture through the injected dialer, and
/// bytes copy both ways; closing the guest's side then closes the whole
/// tunnel.
#[test]
fn an_allowed_connect_tunnels_bytes_both_ways_and_either_eof_closes_both() {
    let fixture = spawn_echo_fixture();
    let dialer = Arc::new(TestDialer {
        fixture,
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer.clone());
    let handle = guest.tcp_connect(GATEWAY, PORT, 40004);
    let established = guest.poll_until(Duration::from_secs(5), |g| {
        g.tcp_state(handle) == tcp::State::Established
    });
    assert!(established);
    guest.tcp_send(handle, &connect_request("a.example"));

    let got_response = guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle));
    assert!(got_response, "an allowed CONNECT must get a response");
    let resp = guest.tcp_recv(handle);
    assert!(resp.starts_with(b"HTTP/1.1 200"), "got {resp:?}");
    assert_eq!(dialer.dial_calls.load(Ordering::SeqCst), 1);

    guest.tcp_send(handle, b"ping");
    let echoed = guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle));
    assert!(
        echoed,
        "the fixture's echo must come back through the tunnel"
    );
    assert_eq!(guest.tcp_recv(handle), b"ping");

    guest.tcp_close(handle);
    let closed = guest.poll_until(Duration::from_secs(5), |g| g.tcp_done(handle));
    assert!(closed, "closing the guest side must close the whole tunnel");
    session.stop();
    join_within(session, Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

/// The ledger is the review surface synod's grant sells as `audit: true`,
/// and this proxy is its only writer: a refusal in the department's own
/// words, the tunnel that was allowed, and what that tunnel carried.
#[test]
fn the_ledger_records_the_refusal_the_tunnel_and_its_byte_counts() {
    let ledger = std::env::temp_dir().join(format!(
        "guest-net-ledger-test-{}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&ledger);
    let egress = Egress {
        policy: Arc::new(exarch::net_policy::NetPolicy {
            hosts: std::iter::once(Host::parse("a.example").expect("a valid fixture host"))
                .collect(),
            search: true,
        }),
        audit: exarch::egress::AuditLog::for_test(&ledger),
    };
    let dialer = Arc::new(TestDialer {
        fixture: spawn_echo_fixture(),
        ..TestDialer::default()
    });
    let (mut guest, session) = harness_with(dialer, egress);

    let denied = guest.tcp_connect(GATEWAY, PORT, 40011);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(denied)
            == tcp::State::Established)
    );
    guest.tcp_send(denied, &connect_request("off-list.example"));
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(denied)));
    assert!(guest.tcp_recv(denied).starts_with(b"HTTP/1.1 403"));

    let allowed = guest.tcp_connect(GATEWAY, PORT, 40012);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(allowed)
            == tcp::State::Established)
    );
    guest.tcp_send(allowed, &connect_request("a.example"));
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(allowed)));
    assert!(guest.tcp_recv(allowed).starts_with(b"HTTP/1.1 200"));
    guest.tcp_send(allowed, b"ping");
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(allowed)));
    assert_eq!(guest.tcp_recv(allowed), b"ping");
    guest.tcp_close(allowed);
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_done(allowed)));

    session.stop();
    join_within(session, Duration::from_secs(5))
        .unwrap()
        .unwrap();

    let text = std::fs::read_to_string(&ledger).expect("the ledger is written");
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON on its own"))
        .collect();
    assert_eq!(lines.len(), 3, "one refusal, one tunnel, one close: {text}");

    assert_eq!(lines[0]["host"], "off-list.example");
    assert_eq!(lines[0]["allowed"], false);
    assert_eq!(
        lines[0]["note"],
        serde_json::json!(exarch::net_policy::refusal("off-list.example"))
    );

    assert_eq!(lines[1]["host"], "a.example");
    assert_eq!(lines[1]["allowed"], true);
    // The vetted address the dialer was handed, not the loopback fixture
    // this test's double actually connected to.
    assert_eq!(lines[1]["addr"], "93.184.216.34:443");
    assert!(lines[1]["note"].is_null());

    assert_eq!(lines[2]["host"], "a.example");
    assert_eq!(lines[2]["note"], "closed");
    assert_eq!(lines[2]["up"], 4);
    assert_eq!(lines[2]["down"], 4);
    let _ = std::fs::remove_file(&ledger);
}

/// The fixed live-tunnel cap refuses the next connection at accept, before
/// any worker exists, and frees a slot the moment one tunnel ends.
#[test]
fn capacity_rejects_the_next_connection_and_frees_after_one_tunnel_ends() {
    let fixture = spawn_echo_fixture();
    let dialer = Arc::new(TestDialer {
        fixture,
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer);

    let mut handles = Vec::new();
    for i in 0..TUNNEL_CAP {
        let local_port = 41000 + u16::try_from(i).unwrap();
        let handle = guest.tcp_connect(GATEWAY, PORT, local_port);
        assert!(
            guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(handle)
                == tcp::State::Established),
            "connection {i} must be accepted"
        );
        guest.tcp_send(handle, &connect_request("a.example"));
        assert!(
            guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle)),
            "connection {i} must get a response"
        );
        assert!(guest.tcp_recv(handle).starts_with(b"HTTP/1.1 200"));
        handles.push(handle);
    }

    let over_cap = guest.tcp_connect(GATEWAY, PORT, 42000);
    let refused = guest.poll_until(Duration::from_secs(5), |g| {
        matches!(
            g.tcp_state(over_cap),
            tcp::State::Closed | tcp::State::TimeWait
        )
    });
    assert!(
        refused,
        "a connection past the cap must be refused at accept, before any worker runs"
    );

    guest.tcp_close(handles[0]);
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_done(handles[0])));

    let freed = guest.tcp_connect(GATEWAY, PORT, 42001);
    let accepted = guest.poll_until(Duration::from_secs(5), |g| {
        g.tcp_state(freed) == tcp::State::Established
    });
    assert!(accepted, "ending one tunnel must free a slot for the next");
    guest.tcp_send(freed, &connect_request("a.example"));
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(freed)));
    assert!(guest.tcp_recv(freed).starts_with(b"HTTP/1.1 200"));

    session.stop();
    join_within(session, Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

#[test]
fn stop_during_an_incomplete_connect_ends_the_session_promptly() {
    let (mut guest, session) = harness(Arc::new(TestDialer::default()));
    let handle = guest.tcp_connect(GATEWAY, PORT, 40005);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(handle)
            == tcp::State::Established)
    );
    guest.tcp_send(handle, b"CONNECT a.example:443 HTTP/1.1\r\n"); // no terminating blank line
    guest.poll();
    session.stop();
    let result = join_within(session, Duration::from_secs(5));
    assert!(
        result.is_some(),
        "join must return promptly, not wait out the handshake deadline"
    );
    assert!(result.unwrap().is_ok());
}

#[test]
fn stop_while_a_worker_is_dialling_ends_the_session_once_the_dial_returns() {
    let fixture = spawn_echo_fixture();
    let dialer = Arc::new(TestDialer {
        fixture,
        dial_delay: Duration::from_millis(300),
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer);
    let handle = guest.tcp_connect(GATEWAY, PORT, 40006);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(handle)
            == tcp::State::Established)
    );
    guest.tcp_send(handle, &connect_request("a.example"));
    guest.poll();
    thread::sleep(Duration::from_millis(50)); // let the worker get into the dial
    session.stop();
    let result = join_within(session, Duration::from_secs(5));
    assert!(
        result.is_some(),
        "a bounded dial must not hold join up indefinitely"
    );
    assert!(result.unwrap().is_ok());
}

#[test]
fn stop_during_an_idle_established_tunnel_ends_the_session_promptly() {
    let fixture = spawn_echo_fixture();
    let dialer = Arc::new(TestDialer {
        fixture,
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer);
    let handle = guest.tcp_connect(GATEWAY, PORT, 40007);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(handle)
            == tcp::State::Established)
    );
    guest.tcp_send(handle, &connect_request("a.example"));
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle)));
    assert!(guest.tcp_recv(handle).starts_with(b"HTTP/1.1 200"));

    session.stop();
    let result = join_within(session, Duration::from_secs(5));
    assert!(
        result.is_some(),
        "an idle tunnel must not hold join up — the worker is blocked reading the pipe"
    );
    assert!(result.unwrap().is_ok());
}

/// The worker→guest ring is filled on purpose by a fixture that floods
/// and a guest that never drains, so the worker's write genuinely blocks;
/// `stop` must still wake it.
#[test]
fn stop_while_the_worker_to_guest_direction_is_blocked_ends_the_session_promptly() {
    let fixture = spawn_flood_fixture();
    let dialer = Arc::new(TestDialer {
        fixture,
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer);
    let handle = guest.tcp_connect(GATEWAY, PORT, 40008);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(handle)
            == tcp::State::Established)
    );
    guest.tcp_send(handle, &connect_request("a.example"));
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle)));
    assert!(guest.tcp_recv(handle).starts_with(b"HTTP/1.1 200"));

    // Never drain again: let the flood fill the ring and the socket
    // window from here on, without any further polling from this side.
    thread::sleep(Duration::from_millis(500));

    session.stop();
    let result = join_within(session, Duration::from_secs(5));
    assert!(
        result.is_some(),
        "a worker blocked writing to a full ring must wake on stop, not hang join"
    );
    assert!(result.unwrap().is_ok());
}

#[test]
fn a_worker_panic_becomes_a_named_session_failure() {
    let dialer = Arc::new(TestDialer {
        panic_on_resolve: true,
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer);
    let handle = guest.tcp_connect(GATEWAY, PORT, 40009);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(handle)
            == tcp::State::Established)
    );
    guest.tcp_send(handle, &connect_request("a.example"));
    let closed = guest.poll_until(Duration::from_secs(5), |g| g.tcp_peer_closed(handle));
    assert!(closed, "a panicked worker must still tear its socket down");

    session.stop();
    let result = join_within(session, Duration::from_secs(5)).expect("join must return");
    let err =
        result.expect_err("a worker panic must surface as a named failure, not a detached panic");
    assert!(err.contains("panicked"), "got {err:?}");
}

#[test]
#[cfg(target_os = "linux")]
fn repeated_start_stop_leaves_thread_count_stable() {
    fn thread_count() -> usize {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("Threads:")
                        .map(|n| n.trim().parse().unwrap())
                })
            })
            .unwrap_or(0)
    }

    let before = thread_count();
    for _ in 0..5 {
        let (guest, session) = harness(Arc::new(TestDialer::default()));
        drop(guest);
        session.stop();
        join_within(session, Duration::from_secs(5))
            .unwrap()
            .unwrap();
    }
    let after = thread_count();
    assert!(
        after <= before + 2,
        "threads leaked across repeated start/stop: before={before} after={after}"
    );
}
