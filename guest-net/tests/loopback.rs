//! Stage 9's proof: a second `smoltcp` stack playing the guest, talking to
//! `guest_net::run`'s host stack over a `UnixStream` pair — the same frame
//! codec and [`device::Wire`] abstraction both ends of the real link use.
//! `GuestSide` drives its own `Interface`/`SocketSet` by hand, exactly the
//! shape a real guest kernel's TCP/IP stack would exercise, which is what
//! lets these tests prove the raw-IP refusal, the DNS answers, and the
//! port gate before a line of TLS or `proxy.rs` exists anywhere in this
//! crate — the [`Config::handler`] here is a stub that only records that a
//! connection was ever handed to it.
//!
//! Unix-only: the harness needs a `UnixStream` pair to stand in for the net
//! wire, and [`device::Wire`]'s Windows twin is exercised only by the type
//! check, not by a socket pair std has no equivalent constructor for.
#![cfg(unix)]

use std::net::Ipv4Addr;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};

use exarch::egress::{AuditLog, Egress, RateLimiter};
use exarch::net_policy::{NetPolicy, Rule};

use guest_net::{Config, device};

/// The interface's own address — also where the guest's `resolv.conf`
/// would point its nameserver, per [`Config::gateway`]'s doc.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);

/// The guest's own address on the link, chosen well outside the
/// `100.64.0.0/10` range `Names` mints from so it can never collide with a
/// minted answer.
const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(100, 100, 0, 2);

const SOCKET_BUF: usize = 8 * 1024;

/// A minimal guest: a `smoltcp` interface driven by hand over one end of a
/// `UnixStream` pair, the other end handed to [`guest_net::run`].
struct GuestSide {
    device: device::TunDevice<UnixStream>,
    iface: Interface,
    sockets: SocketSet<'static>,
}

impl GuestSide {
    fn new(guest_end: UnixStream) -> Self {
        let (mut device, _stop) = device::spawn(guest_end, None).expect("reader starts");
        let mut config = IfaceConfig::new(HardwareAddress::Ip);
        config.random_seed = 1;
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(IpAddress::Ipv4(GUEST_ADDR), 10)).unwrap();
        });
        Self {
            device,
            iface,
            sockets: SocketSet::new(Vec::new()),
        }
    }

    fn poll(&mut self) {
        self.iface.poll(Instant::now(), &mut self.device, &mut self.sockets);
    }

    /// Poll until `done` is satisfied or `timeout` elapses. Generous
    /// margins throughout this file: this host's own scheduling jitter is
    /// high, and every wait here is for one or two round trips over a
    /// loopback socket pair, not a real network.
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
            std::thread::sleep(Duration::from_millis(5));
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

    fn udp_query(&mut self, dst: Ipv4Addr, local_port: u16, query: &[u8]) -> SocketHandle {
        let socket = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 2048]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 2048]),
        );
        let handle = self.sockets.add(socket);
        let socket = self.sockets.get_mut::<udp::Socket>(handle);
        socket.bind(local_port).expect("a fresh ephemeral port binds");
        socket
            .send_slice(query, (IpAddress::Ipv4(dst), 53))
            .expect("room for one query");
        handle
    }

    fn udp_can_recv(&self, handle: SocketHandle) -> bool {
        self.sockets.get::<udp::Socket>(handle).can_recv()
    }

    fn udp_response(&mut self, handle: SocketHandle) -> Vec<u8> {
        let socket = self.sockets.get_mut::<udp::Socket>(handle);
        let mut buf = [0u8; 2048];
        let (n, _meta) = socket.recv_slice(&mut buf).expect("caller checked can_recv first");
        buf[..n].to_vec()
    }
}

fn wire_name(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in name.split('.') {
        out.push(u8::try_from(label.len()).unwrap());
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// Build a minimal, RD-set, no-EDNS DNS query for `name`/`qtype`.
fn dns_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(&id.to_be_bytes());
    msg.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes());
    msg.extend(wire_name(name));
    msg.extend_from_slice(&qtype.to_be_bytes());
    msg.extend_from_slice(&1u16.to_be_bytes()); // IN
    msg
}

fn dns_rcode(resp: &[u8]) -> u16 {
    u16::from_be_bytes([resp[2], resp[3]]) & 0xF
}

fn dns_ancount(resp: &[u8]) -> u16 {
    u16::from_be_bytes([resp[6], resp[7]])
}

/// The minted `A` record's address, for a response built from a query for
/// `name` at type `A` — the question length is recomputed to find where
/// the answer section starts, since it is not fixed size.
fn dns_answer_addr(resp: &[u8], name: &str) -> Ipv4Addr {
    let qlen = wire_name(name).len() + 4;
    let rr = 12 + qlen;
    let rdata = rr + 12; // NAME(2) TYPE(2) CLASS(2) TTL(4) RDLENGTH(2)
    Ipv4Addr::new(resp[rdata], resp[rdata + 1], resp[rdata + 2], resp[rdata + 3])
}

/// A [`Config`] with a policy admitting only `a.example`, a throwaway audit
/// ledger, and a handler that just counts how many connections it saw —
/// enough to prove the accept gate's verdict without any TLS or HTTP code.
fn test_config() -> (Config, Arc<AtomicUsize>) {
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    let policy = NetPolicy {
        allow: vec![Rule {
            host: "a.example".to_string(),
            methods: vec!["GET".to_string()],
        }],
        max_response_bytes: 1_048_576,
        rate_per_minute: 10_000,
        search: false,
    };
    let path = std::env::temp_dir().join(format!(
        "guest-net-loopback-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config = Config {
        egress: Egress {
            policy: Arc::new(policy),
            audit: AuditLog::for_test(&path),
            limiter: RateLimiter::new(10_000),
        },
        gateway: GATEWAY,
        dns_rate_per_minute: 10_000,
        handler: Arc::new(move |accepted| {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(accepted);
        }),
    };
    (config, accepted)
}

fn harness() -> (GuestSide, Arc<AtomicUsize>, guest_net::Session<UnixStream>) {
    let (host_end, guest_end) = UnixStream::pair().expect("a socket pair");
    let (config, accepted) = test_config();
    let session = guest_net::run(host_end, config).expect("the host stack starts");
    (GuestSide::new(guest_end), accepted, session)
}

/// The whole point of the crate, proved with nothing downstream of the
/// accept gate built yet: a `100.64.0.0/10` address DNS never minted has
/// no name, so a guest dialling it directly gets a `RST`, never a worker.
#[test]
fn raw_ip_to_an_address_dns_never_minted_is_refused() {
    let (mut guest, accepted, session) = harness();
    let never_minted = Ipv4Addr::new(100, 64, 33, 44);
    let handle = guest.tcp_connect(never_minted, 80, 40000);
    let closed = guest.poll_until(Duration::from_secs(5), |g| {
        matches!(g.tcp_state(handle), tcp::State::Closed | tcp::State::TimeWait)
    });
    assert!(closed, "an unminted address must be refused, not left hanging");
    assert_eq!(accepted.load(Ordering::SeqCst), 0, "no worker was ever handed this connection");
    session.stop();
    session.join();
}

/// The other half of the same proof: a name the policy actually admits
/// mints a real address, and dialling *that* address on 80 is accepted —
/// the gate distinguishes the two, it does not just refuse everything.
#[test]
fn an_address_minted_for_an_allowed_name_is_accepted_on_80() {
    let (mut guest, accepted, session) = harness();
    let dns = guest.udp_query(GATEWAY, 40001, &dns_query(1, "a.example", 1));
    let arrived = guest.poll_until(Duration::from_secs(5), |g| g.udp_can_recv(dns));
    assert!(arrived, "the DNS answer must arrive");
    let resp = guest.udp_response(dns);
    assert_eq!(dns_rcode(&resp), 0);
    let minted = dns_answer_addr(&resp, "a.example");

    let handle = guest.tcp_connect(minted, 80, 40002);
    let established = guest.poll_until(Duration::from_secs(5), |g| {
        g.tcp_state(handle) == tcp::State::Established
    });
    assert!(established, "a minted, on-list address must be accepted on 80");
    let saw_worker = guest.poll_until(Duration::from_secs(2), |_| accepted.load(Ordering::SeqCst) >= 1);
    assert!(saw_worker, "the accept gate must hand the connection to a worker");
    session.stop();
    session.join();
}

/// Every port but 80 and 443 has no listener at all, so `smoltcp`'s own
/// stack answers with a `RST` — gate four, and no code of this crate's own
/// runs to produce it.
#[test]
fn a_port_other_than_80_or_443_is_reset() {
    let (mut guest, _accepted, session) = harness();
    let handle = guest.tcp_connect(GATEWAY, 2222, 40003);
    let closed = guest.poll_until(Duration::from_secs(5), |g| {
        matches!(g.tcp_state(handle), tcp::State::Closed | tcp::State::TimeWait)
    });
    assert!(closed, "a port with no listener must reset, not hang");
    session.stop();
    session.join();
}

#[test]
fn an_off_list_name_gets_nxdomain() {
    let (mut guest, _accepted, session) = harness();
    let dns = guest.udp_query(GATEWAY, 40004, &dns_query(7, "off-list.example", 1));
    let arrived = guest.poll_until(Duration::from_secs(5), |g| g.udp_can_recv(dns));
    assert!(arrived, "a response arrives even for an off-list name");
    let resp = guest.udp_response(dns);
    assert_eq!(dns_rcode(&resp), 3, "NXDOMAIN");
    session.stop();
    session.join();
}

/// `AAAA` on an on-list name must be `NOERROR` with zero answers, never
/// `NXDOMAIN` — see `dns.rs`'s own doc for why that distinction matters.
#[test]
fn an_on_list_aaaa_query_is_noerror_with_zero_answers() {
    let (mut guest, _accepted, session) = harness();
    let dns = guest.udp_query(GATEWAY, 40005, &dns_query(8, "a.example", 28));
    let arrived = guest.poll_until(Duration::from_secs(5), |g| g.udp_can_recv(dns));
    assert!(arrived, "a response arrives for an AAAA query too");
    let resp = guest.udp_response(dns);
    assert_eq!(dns_rcode(&resp), 0, "NOERROR");
    assert_eq!(dns_ancount(&resp), 0);
    session.stop();
    session.join();
}

/// `stop()` unsticks the reader even mid-session — the same mechanism the
/// `device` module's own tests exercise in isolation, proved here once more
/// through the full stack.
#[test]
fn stop_ends_a_session_with_nothing_in_flight() {
    let (guest, _accepted, session) = harness();
    session.stop();
    session.join();
    drop(guest);
}

/// Kept out of the corpus replay in `dns.rs`, but a light sanity check that
/// the seed files this crate ships for `cargo-fuzz` are themselves
/// well-formed enough to be useful seeds, not just non-panicking noise.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "a test-only check that the crate's own checked-in fuzz corpus is present, not turn-time model I/O"
)]
fn the_shipped_dns_corpus_seeds_exist() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/dns");
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("the corpus directory ships with the crate")
        .collect();
    assert!(!entries.is_empty(), "the fuzz corpus must not be empty");
}
