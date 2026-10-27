#![allow(clippy::disallowed_methods, reason = "test code")]

//! A guest smoltcp stack speaks production framing over a `UnixStream` pair.
//! Resolution is vetted normally; injected dials reach only local fixtures.
//! Tests are serial because thread counts are process-wide.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};

use ral_core::egress::{Host, NetPolicy, refusal};

use guest_abi::link::{LINK, PROXY_PORT};
use netgate::{AuditLog, Config, Dialer, LEDGER};

const SOCKET_BUF: usize = 8 * 1024;
/// Must match the private `stack::TUNNEL_CAP`.
const TUNNEL_CAP: usize = 32;

struct GuestSide {
    device: netgate::TunDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
}

impl GuestSide {
    fn new(guest_end: UnixStream) -> Self {
        let (mut device, _stop) =
            netgate::spawn_device(guest_end, Arc::new(|| {})).expect("reader starts");
        let mut config = IfaceConfig::new(HardwareAddress::Ip);
        config.random_seed = 1;
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(LINK.address.into(), LINK.prefix))
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

    fn tcp_tunnel(&mut self, local_port: u16) -> (SocketHandle, Vec<u8>) {
        let handle = self.tcp_connect(LINK.gateway, PROXY_PORT, local_port);
        assert!(self.poll_until(Duration::from_secs(5), |g| {
            g.tcp_state(handle) == tcp::State::Established
        }));
        self.tcp_send(handle, &connect_request("a.example"));
        let mut response = Vec::new();
        assert!(self.poll_until(Duration::from_secs(5), |g| {
            response.extend(g.tcp_recv(handle));
            response.windows(4).any(|w| w == b"\r\n\r\n")
        }));
        assert!(response.starts_with(b"HTTP/1.1 200"), "{response:?}");
        let end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        (handle, response.split_off(end))
    }

    /// Peer FIN leaves the guest free to send; a refusal closes only the host half.
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

    /// Count TIME-WAIT as done instead of waiting 2MSL.
    fn tcp_done(&self, handle: SocketHandle) -> bool {
        matches!(
            self.tcp_state(handle),
            tcp::State::Closed | tcp::State::TimeWait
        )
    }
}

struct TestDialer {
    fixture: SocketAddr,
    resolve_to: SocketAddr,
    dial_delay: Duration,
    panic_on_resolve: bool,
    resolve_calls: AtomicUsize,
    dial_calls: AtomicUsize,
}

/// The default fixture has no listener, so an unexpected dial fails loudly.
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

impl Dialer for TestDialer {
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

fn spawn_flood_fixture() -> (
    SocketAddr,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Receiver<std::io::Result<()>>,
) {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (addr, done) = spawn_bounded_fixture(move |mut stream| {
        let chunk = [0u8; 8192];
        for _ in 0..32 {
            stream.write_all(&chunk)?;
        }
        let _ = ready_tx.send(());
        loop {
            if let Err(e) = stream.write_all(&chunk) {
                return if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) {
                    Err(e)
                } else {
                    Ok(())
                };
            }
        }
    });
    (addr, ready_rx, done)
}

fn spawn_bounded_fixture<T: Send + 'static>(
    serve: impl FnOnce(TcpStream) -> std::io::Result<T> + Send + 'static,
) -> (SocketAddr, std::sync::mpsc::Receiver<std::io::Result<T>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "fixture was never dialled",
                            ));
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => return Err(e),
                }
            };
            // On macOS an accepted socket inherits the listener's O_NONBLOCK.
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            serve(stream)
        })();
        let _ = tx.send(result);
    });
    (addr, rx)
}

fn connect_request(host: &str) -> Vec<u8> {
    format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\nUser-Agent: test\r\n\r\n")
        .into_bytes()
}

fn harness(dialer: Arc<dyn Dialer>) -> (GuestSide, netgate::Session) {
    harness_with(Config {
        policy: Arc::new(NetPolicy::for_test(&["example.com", "a.example"])),
        audit: AuditLog::in_scratch(),
        dialer,
    })
}

fn harness_with(config: Config) -> (GuestSide, netgate::Session) {
    let (host_end, guest_end) = UnixStream::pair().expect("a socket pair");
    let session = netgate::run(host_end, config).expect("the host stack starts");
    (GuestSide::new(guest_end), session)
}

fn join_within(session: netgate::Session, timeout: Duration) -> Option<Result<(), String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(session.join());
    });
    rx.recv_timeout(timeout).ok()
}

/// New lifecycle regressions must also clean up when an assertion fails.
struct StopOnDrop(Option<netgate::Session>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let session = self.0.take().unwrap();
        session.stop();
        let result = join_within(session, Duration::from_secs(5));
        if !thread::panicking() {
            assert_eq!(result, Some(Ok(())), "session teardown must finish");
        }
    }
}

#[test]
#[serial_test::serial]
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
#[serial_test::serial]
fn a_port_other_than_3128_has_no_listener() {
    let (mut guest, session) = harness(Arc::new(TestDialer::default()));
    let handle = guest.tcp_connect(LINK.gateway, 9999, 40001);
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
#[serial_test::serial]
fn a_denied_connect_never_dials() {
    let dialer = Arc::new(TestDialer::default());
    let (mut guest, session) = harness(dialer.clone());
    let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40002);
    let established = guest.poll_until(Duration::from_secs(5), |g| {
        g.tcp_state(handle) == tcp::State::Established
    });
    assert!(established, "the proxy port must accept the handshake");
    guest.tcp_send(handle, &connect_request("off-list.example"));
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle)));
    let refused = String::from_utf8(guest.tcp_recv(handle)).expect("the status line is UTF-8");
    assert!(refused.starts_with("HTTP/1.1 403"), "got: {refused}");
    assert!(
        refused.contains(&refusal("off-list.example")),
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

#[test]
#[serial_test::serial]
fn the_authority_confusion_regression_is_refused_before_any_dial() {
    let dialer = Arc::new(TestDialer::default());
    let (mut guest, session) = harness(dialer.clone());
    let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40003);
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

#[test]
#[serial_test::serial]
fn an_allowed_host_that_resolves_to_a_link_local_address_never_dials() {
    let dialer = Arc::new(TestDialer {
        fixture: spawn_echo_fixture(),
        resolve_to: "169.254.169.254:443".parse().unwrap(),
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer.clone());
    let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40010);
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

#[test]
#[serial_test::serial]
fn an_allowed_connect_completes_a_duplex_echo_exchange() {
    let fixture = spawn_echo_fixture();
    let dialer = Arc::new(TestDialer {
        fixture,
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer.clone());
    let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40004);
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
    assert!(closed, "the completed echo exchange must close both halves");
    session.stop();
    join_within(session, Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

#[test]
#[serial_test::serial]
fn origin_fin_preserves_the_guest_upload_until_its_own_fin() {
    const RESPONSE: &[u8] = b"origin is done sending";
    const PREFIX: &[u8] = b"upload before origin FIN; ";
    const SUFFIX: &[u8] = b"upload after origin FIN";
    let (fixture, received) = spawn_bounded_fixture(|mut stream| {
        let mut prefix = vec![0; PREFIX.len()];
        stream.read_exact(&mut prefix)?;
        stream.write_all(RESPONSE)?;
        stream.shutdown(Shutdown::Write)?;
        stream.read_to_end(&mut prefix)?;
        Ok(prefix)
    });
    let state = ral_core::own::OwnDir::for_test("netgate-half-close").unwrap();
    let config = Config {
        policy: Arc::new(NetPolicy {
            hosts: std::iter::once(Host::parse("a.example").unwrap()).collect(),
            search: true,
        }),
        audit: AuditLog::for_test(&state),
        dialer: Arc::new(TestDialer {
            fixture,
            ..TestDialer::default()
        }),
    };
    let (mut guest, session) = harness_with(config);
    let session = StopOnDrop(Some(session));
    let (handle, mut output) = guest.tcp_tunnel(43000);
    guest.tcp_send(handle, PREFIX);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| {
            output.extend(g.tcp_recv(handle));
            g.tcp_peer_closed(handle) && !g.tcp_can_recv(handle)
        }),
        "origin FIN must reach a guest that has not closed its upload"
    );
    assert_eq!(output, RESPONSE);
    assert_eq!(guest.tcp_state(handle), tcp::State::CloseWait);
    guest.tcp_send(handle, SUFFIX);
    guest.tcp_close(handle);
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_done(handle)));
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap(),
        [PREFIX, SUFFIX].concat(),
        "origin must receive the suffix sent after its own FIN"
    );
    drop(session);

    let text = String::from_utf8(state.read(LEDGER).unwrap()).unwrap();
    let records: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2, "one allow and one close: {text}");
    assert_eq!(records[0]["allowed"], true);
    assert_eq!(records[1]["note"], "closed");
    assert_eq!(records[1]["up"], PREFIX.len() + SUFFIX.len());
    assert_eq!(records[1]["down"], RESPONSE.len());
}

#[test]
#[serial_test::serial]
fn guest_fin_delivers_a_response_larger_than_the_tunnel_buffers() {
    const REQUEST: &[u8] = b"respond only after request EOF";
    let response: Vec<u8> = (0..251).cycle().take(512 * 1024).collect();
    let expected = response.clone();
    let (fixture, received) = spawn_bounded_fixture(move |mut stream| {
        let mut request = Vec::new();
        stream.read_to_end(&mut request)?;
        stream.write_all(&response)?;
        stream.shutdown(Shutdown::Write)?;
        Ok(request)
    });
    let (mut guest, session) = harness(Arc::new(TestDialer {
        fixture,
        ..TestDialer::default()
    }));
    let _session = StopOnDrop(Some(session));
    let (handle, mut output) = guest.tcp_tunnel(43001);
    assert!(output.is_empty(), "origin must wait for request EOF");
    guest.tcp_send(handle, REQUEST);
    guest.tcp_close(handle);
    assert!(
        guest.poll_until(Duration::from_secs(10), |g| {
            output.extend(g.tcp_recv(handle));
            g.tcp_done(handle) && !g.tcp_can_recv(handle)
        }),
        "guest FIN must leave the download alive through its final bytes and FIN"
    );
    assert_eq!(output, expected, "the entire response must arrive in order");
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap(),
        REQUEST
    );
}

#[test]
#[serial_test::serial]
fn guest_reset_wakes_blocked_output_and_releases_its_slot() {
    let (fixture, ready, finished) = spawn_flood_fixture();
    let (mut guest, session) = harness(Arc::new(TestDialer {
        fixture,
        ..TestDialer::default()
    }));
    let _session = StopOnDrop(Some(session));
    let (flood, _) = guest.tcp_tunnel(43002);
    assert!(guest.poll_until(Duration::from_secs(5), |g| {
        g.sockets.get::<tcp::Socket>(flood).recv_queue() == SOCKET_BUF
    }));
    ready.recv_timeout(Duration::from_secs(5)).unwrap();

    // Incomplete CONNECTs hold every other slot without dialing the fixture.
    for i in 1..TUNNEL_CAP {
        let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 43100 + u16::try_from(i).unwrap());
        assert!(guest.poll_until(Duration::from_secs(5), |g| {
            g.tcp_state(handle) == tcp::State::Established
        }));
    }
    let rejected = guest.tcp_connect(LINK.gateway, PROXY_PORT, 43200);
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_done(rejected)));
    guest.sockets.get_mut::<tcp::Socket>(flood).abort();
    guest.poll();

    // Retry admission: the reset worker may need a few polls to finish.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut port = 43201;
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "RST must release its slot and the host must keep serving connections"
        );
        let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, port);
        assert!(guest.poll_until(Duration::from_secs(1), |g| {
            g.tcp_state(handle) == tcp::State::Established || g.tcp_done(handle)
        }));
        if guest.tcp_state(handle) == tcp::State::Established {
            guest.tcp_send(handle, &connect_request("off-list.example"));
            let mut response = Vec::new();
            assert!(guest.poll_until(Duration::from_secs(1), |g| {
                response.extend(g.tcp_recv(handle));
                response.windows(4).any(|w| w == b"\r\n\r\n") || g.tcp_done(handle)
            }));
            if !response.is_empty() {
                assert!(response.starts_with(b"HTTP/1.1 403"));
                break;
            }
        }
        port += 1;
    }
    finished
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

#[test]
#[serial_test::serial]
fn the_ledger_records_the_refusal_the_tunnel_and_its_byte_counts() {
    let state = ral_core::own::OwnDir::for_test("netgate-ledger").expect("a ledger directory");
    let config = Config {
        policy: Arc::new(NetPolicy {
            hosts: std::iter::once(Host::parse("a.example").expect("a valid fixture host"))
                .collect(),
            search: true,
        }),
        audit: AuditLog::for_test(&state),
        dialer: Arc::new(TestDialer {
            fixture: spawn_echo_fixture(),
            ..TestDialer::default()
        }),
    };
    let (mut guest, session) = harness_with(config);

    let denied = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40011);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(denied)
            == tcp::State::Established)
    );
    guest.tcp_send(denied, &connect_request("off-list.example"));
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(denied)));
    assert!(guest.tcp_recv(denied).starts_with(b"HTTP/1.1 403"));

    let allowed = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40012);
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

    let text = String::from_utf8(state.read(LEDGER).expect("the ledger is written"))
        .expect("the ledger is text");
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON on its own"))
        .collect();
    assert_eq!(lines.len(), 3, "one refusal, one tunnel, one close: {text}");

    assert_eq!(lines[0]["host"], "off-list.example");
    assert_eq!(lines[0]["allowed"], false);
    assert_eq!(
        lines[0]["note"],
        serde_json::json!(refusal("off-list.example"))
    );

    assert_eq!(lines[1]["host"], "a.example");
    assert_eq!(lines[1]["allowed"], true);
    // Record the vetted address, not the fixture the test double actually dialled.
    assert_eq!(lines[1]["addr"], "93.184.216.34:443");
    assert!(lines[1]["note"].is_null());

    assert_eq!(lines[2]["host"], "a.example");
    assert_eq!(lines[2]["note"], "closed");
    assert_eq!(lines[2]["up"], 4);
    assert_eq!(lines[2]["down"], 4);
}

#[test]
#[serial_test::serial]
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
        let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, local_port);
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

    let over_cap = guest.tcp_connect(LINK.gateway, PROXY_PORT, 42000);
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

    let freed = guest.tcp_connect(LINK.gateway, PROXY_PORT, 42001);
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
#[serial_test::serial]
fn stop_during_an_incomplete_connect_ends_the_session_promptly() {
    let (mut guest, session) = harness(Arc::new(TestDialer::default()));
    let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40005);
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
#[serial_test::serial]
fn stop_while_a_worker_is_dialling_ends_the_session_once_the_dial_returns() {
    let fixture = spawn_echo_fixture();
    let dialer = Arc::new(TestDialer {
        fixture,
        dial_delay: Duration::from_millis(300),
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer);
    let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40006);
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
#[serial_test::serial]
fn stop_during_an_idle_established_tunnel_ends_the_session_promptly() {
    let fixture = spawn_echo_fixture();
    let dialer = Arc::new(TestDialer {
        fixture,
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer);
    let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40007);
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

#[test]
#[serial_test::serial]
fn stop_while_the_worker_to_guest_direction_is_blocked_ends_the_session_promptly() {
    let (fixture, ready, finished) = spawn_flood_fixture();
    let dialer = Arc::new(TestDialer {
        fixture,
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer);
    let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40008);
    assert!(
        guest.poll_until(Duration::from_secs(5), |g| g.tcp_state(handle)
            == tcp::State::Established)
    );
    guest.tcp_send(handle, &connect_request("a.example"));
    assert!(guest.poll_until(Duration::from_secs(5), |g| g.tcp_can_recv(handle)));
    assert!(guest.tcp_recv(handle).starts_with(b"HTTP/1.1 200"));

    assert!(guest.poll_until(Duration::from_secs(5), |g| {
        g.sockets.get::<tcp::Socket>(handle).recv_queue() == SOCKET_BUF
    }));
    ready.recv_timeout(Duration::from_secs(5)).unwrap();

    session.stop();
    let result = join_within(session, Duration::from_secs(5));
    assert!(
        result.is_some(),
        "a worker blocked writing to a full ring must wake on stop, not hang join"
    );
    assert!(result.unwrap().is_ok());
    finished
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
}

#[test]
#[serial_test::serial]
fn a_worker_panic_becomes_a_named_session_failure() {
    let dialer = Arc::new(TestDialer {
        panic_on_resolve: true,
        ..TestDialer::default()
    });
    let (mut guest, session) = harness(dialer);
    let handle = guest.tcp_connect(LINK.gateway, PROXY_PORT, 40009);
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
#[serial_test::serial]
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
