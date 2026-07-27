//! The core thread: the one owner of `smoltcp`'s [`Interface`] and
//! [`SocketSet`], and of the accept gate the whole crate exists to enforce.
//!
//! Everything downstream of the gate — TLS, the intercepting proxy, the
//! upstream fetch — is `proxy.rs`'s business, reached only through the
//! [`Accepted`] value a connection's [`Handler`] receives. This module
//! never reads a byte of application data; it only decides whether a
//! connection *gets* a worker at all.
//!
//! # The gate
//!
//! The interface listens on `100.64.0.0/10` with `set_any_ip(true)`
//! (verified against the pinned `smoltcp` 0.13.1: `has_ip_addr` short-
//! circuits to `true` whenever `any_ip` is set, so no destination-NAT
//! fallback is needed), and a small pool of `tcp::Socket`s sits listening
//! on `IpListenEndpoint { addr: None, port: 80 | 443 }` apiece. A listener
//! leaving the `Listen` state *is* the accept: [`Names::name_of`] on
//! [`tcp::Socket::local_endpoint`]'s address answers whether DNS ever
//! minted this destination for a name at all. No name, no worker — the
//! socket is `abort()`ed (smoltcp answers with a `RST`) before a single
//! byte of the connection is read. Every other port never gets a listener,
//! so `smoltcp`'s own stack answers those with a `RST` unprompted; this
//! module writes no code for that case; that is gate four.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpListenEndpoint};

use exarch::egress::{Egress, RateLimiter, Record};

use crate::device::{self, TunDevice, Wire};
use crate::names::Names;
use crate::pipe::{self, ProxyPipe, Wake};

/// A connection the accept gate let through: the name DNS minted the
/// guest's destination address for, the port it was dialled on, and the
/// two-ring pipe a worker drives with ordinary blocking I/O.
pub struct Accepted {
    pub name: String,
    pub port: u16,
    pub pipe: ProxyPipe,
}

/// What runs on a fresh thread for every accepted connection. `proxy.rs`
/// supplies the real one (TLS termination, the intercepting HTTP proxy);
/// tests supply an echo or no-op.
pub type Handler = Arc<dyn Fn(Accepted) + Send + Sync>;

/// Each `tcp::Socket`'s own send/receive buffers. Distinct from
/// [`pipe`]'s 64 KiB rings — this is `smoltcp`'s TCP window, not the
/// worker-facing pipe — and kept modest since the pipe is where any real
/// buffering happens.
const SOCKET_BUF: usize = 16 * 1024;

/// How many idle listening sockets sit ready on each of ports 80 and 443.
/// Topped up by one every time one accepts, so this is a floor, not a cap.
const LISTENERS_PER_PORT: usize = 4;

const PORTS: [u16; 2] = [80, 443];

/// The longest the core thread ever parks between polls when nothing has
/// rung the doorbell — an upper bound on how stale `smoltcp`'s own timers
/// (retransmit, `TIME-WAIT`) are allowed to get, not a busy-poll interval.
const MAX_WAIT: Duration = Duration::from_millis(200);

/// A condvar the reader thread and every connection's worker ring to wake
/// the core thread promptly, so its poll loop can park on `poll_delay()`
/// instead of ticking on a fixed timer.
struct Doorbell {
    dirty: Mutex<bool>,
    cvar: Condvar,
}

impl Doorbell {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            dirty: Mutex::new(false),
            cvar: Condvar::new(),
        })
    }

    fn ring(&self) {
        *self.dirty.lock().expect("doorbell poisoned") = true;
        self.cvar.notify_one();
    }

    fn wait(&self, timeout: Duration) {
        let mut dirty = self.dirty.lock().expect("doorbell poisoned");
        if !*dirty {
            dirty = self
                .cvar
                .wait_timeout(dirty, timeout)
                .expect("doorbell poisoned")
                .0;
        }
        *dirty = false;
    }
}

/// A caller's handle on a running session: unstick the reader (see
/// [`device`]'s module doc) and, once it and the core thread have both
/// wound down, join the core thread.
pub struct Session<W: Wire> {
    stop: device::Stop<W>,
    core: thread::JoinHandle<()>,
}

impl<W: Wire> Session<W> {
    /// Unblock a reader parked waiting for the guest, and let the core
    /// thread notice the dead wire and end the session on its own.
    pub fn stop(&self) {
        self.stop.stop();
    }

    /// Wait for the core thread to actually end. Blocks until [`Self::stop`]
    /// has been called (from here or elsewhere) or the wire failed on its
    /// own.
    ///
    /// # Panics
    /// If the core thread itself panicked.
    pub fn join(self) {
        self.core.join().expect("guest-net core thread panicked");
    }
}

/// Start a guest-net session: spin up the reader thread over `wire`, bring
/// up the `100.64.0.0/10` interface, and hand the core loop its own
/// thread.
///
/// # Errors
/// Whatever starting the reader thread ([`device::spawn`]) or this
/// module's own core thread reports.
pub fn run<W: Wire>(wire: W, config: crate::Config) -> std::io::Result<Session<W>> {
    let doorbell = Doorbell::new();
    let wake: Wake = {
        let doorbell = doorbell.clone();
        Arc::new(move || doorbell.ring())
    };
    let (device, stop) = device::spawn(wire, Some(wake.clone()))?;

    let core = thread::Builder::new()
        .name("guest-net-core".to_string())
        .spawn(move || Core::new(device, config, doorbell, wake).run())
        .map_err(|e| std::io::Error::other(format!("could not start the guest-net core: {e}")))?;

    Ok(Session { stop, core })
}

struct Core<W: Wire> {
    iface: Interface,
    device: TunDevice<W>,
    sockets: SocketSet<'static>,
    names: Names,
    audit: exarch::egress::AuditLog,
    dns_handle: SocketHandle,
    dns_limiter: RateLimiter,
    /// Idle listeners per port, always at least [`LISTENERS_PER_PORT`] deep.
    pool: HashMap<u16, Vec<SocketHandle>>,
    /// Live connections the gate let through.
    connections: HashMap<SocketHandle, pipe::CoreSide>,
    /// Sockets `abort()`ed or `close()`d whose final packet needed one more
    /// `poll()` to actually go out — reclaimed at the top of the next loop
    /// iteration, never in the same one as the call that closed them.
    pending_removal: Vec<SocketHandle>,
    handler: Handler,
    wake: Wake,
    doorbell: Arc<Doorbell>,
}

impl<W: Wire> Core<W> {
    fn new(
        mut device: TunDevice<W>,
        config: crate::Config,
        doorbell: Arc<Doorbell>,
        wake: Wake,
    ) -> Self {
        let mut iface_config = IfaceConfig::new(HardwareAddress::Ip);
        iface_config.random_seed = rand_seed();
        let mut iface = Interface::new(iface_config, &mut device, Instant::now());
        iface.set_any_ip(true);
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(config.gateway), 10))
                .expect("a fresh interface has room for its one address");
        });

        let mut sockets = SocketSet::new(Vec::new());
        let mut pool = HashMap::new();
        for port in PORTS {
            pool.insert(
                port,
                (0..LISTENERS_PER_PORT)
                    .map(|_| fresh_listener(&mut sockets, port))
                    .collect(),
            );
        }

        let dns_rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 8192]);
        let dns_tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 8192]);
        let mut dns_socket = udp::Socket::new(dns_rx, dns_tx);
        dns_socket
            .bind(IpListenEndpoint {
                addr: None,
                port: 53,
            })
            .expect("a fresh socket can always bind an unused port");
        let dns_handle = sockets.add(dns_socket);

        let Egress {
            policy,
            audit,
            limiter: _,
        } = config.egress;

        Self {
            iface,
            device,
            sockets,
            names: Names::new(policy, audit.clone(), config.gateway),
            audit,
            dns_handle,
            dns_limiter: RateLimiter::new(config.dns_rate_per_minute),
            pool,
            connections: HashMap::new(),
            pending_removal: Vec::new(),
            handler: config.handler,
            wake,
            doorbell,
        }
    }

    fn run(mut self) {
        loop {
            let now = Instant::now();
            self.iface.poll(now, &mut self.device, &mut self.sockets);
            if self.device.failed() {
                break;
            }
            for handle in self.pending_removal.drain(..) {
                self.sockets.remove(handle);
            }

            self.pump_dns();
            self.pump_listeners();
            self.pump_connections();

            let wait = self
                .iface
                .poll_delay(now, &self.sockets)
                .map_or(MAX_WAIT, |d| Duration::from_micros(d.total_micros()).min(MAX_WAIT));
            self.doorbell.wait(wait);
        }
    }

    fn pump_dns(&mut self) {
        let mut buf = [0u8; 512];
        loop {
            let socket = self.sockets.get_mut::<udp::Socket>(self.dns_handle);
            if !socket.can_recv() {
                break;
            }
            let Ok((n, meta)) = socket.recv_slice(&mut buf) else {
                break;
            };
            if let Some(resp) = crate::dns::handle(&buf[..n], &mut self.names, &self.dns_limiter) {
                let _ = socket.send_slice(&resp, meta);
            }
        }
    }

    fn pump_listeners(&mut self) {
        for port in PORTS {
            let handles = self.pool.remove(&port).unwrap_or_default();
            let mut remaining = Vec::with_capacity(handles.len());
            for handle in handles {
                match self.sockets.get::<tcp::Socket>(handle).state() {
                    // Still idle, or a handshake in flight that has not yet
                    // reached (or failed to reach) `Established` — in
                    // either case there is nothing to decide yet, so the
                    // slot is left alone rather than topped up early.
                    tcp::State::Listen | tcp::State::SynReceived => remaining.push(handle),
                    tcp::State::Established => {
                        self.accept(handle, port);
                        remaining.push(fresh_listener(&mut self.sockets, port));
                    }
                    // The handshake ended without ever reaching
                    // `Established` (a reset mid-handshake, say) — nothing
                    // to gate, just reclaim the slot.
                    _ => {
                        self.pending_removal.push(handle);
                        remaining.push(fresh_listener(&mut self.sockets, port));
                    }
                }
            }
            self.pool.insert(port, remaining);
        }
    }

    /// A listening socket just reached `Established`: this is the accept.
    /// Refuse it if its destination address names nothing DNS ever minted;
    /// otherwise spin up a worker thread and hand the connection's pipe to
    /// it.
    fn accept(&mut self, handle: SocketHandle, port: u16) {
        let socket = self.sockets.get::<tcp::Socket>(handle);
        let Some(addr) = socket.local_endpoint().map(|ep| {
            let IpAddress::Ipv4(addr) = ep.addr;
            addr
        }) else {
            self.pending_removal.push(handle);
            return;
        };

        let Some(name) = self.names.name_of(addr).map(str::to_string) else {
            self.audit.record(Record::Connect {
                name: None,
                addr: IpAddr::V4(addr),
                port,
                allowed: false,
                note: Some("no name backs this address"),
            });
            self.sockets.get_mut::<tcp::Socket>(handle).abort();
            self.pending_removal.push(handle);
            return;
        };

        self.audit.record(Record::Connect {
            name: Some(&name),
            addr: IpAddr::V4(addr),
            port,
            allowed: true,
            note: None,
        });
        let (core_side, pipe) = pipe::new_pipe(Some(self.wake.clone()));
        let handler = self.handler.clone();
        let worker_name = name.clone();
        if let Err(e) = thread::Builder::new()
            .name(format!("guest-net-worker-{port}"))
            .spawn(move || {
                handler(Accepted {
                    name: worker_name,
                    port,
                    pipe,
                });
            })
        {
            // `pipe` was consumed by the failed spawn attempt and is
            // already dropped; `core_side` will see that on the next poll
            // and close the socket gracefully, same as a worker that
            // finished normally.
            self.audit.record(Record::Connect {
                name: Some(&name),
                addr: IpAddr::V4(addr),
                port,
                allowed: false,
                note: Some(&format!("worker thread failed to start: {e}")),
            });
        }
        self.connections.insert(handle, core_side);
    }

    fn pump_connections(&mut self) {
        let mut buf = [0u8; 4096];
        let mut done = Vec::new();
        for (&handle, core) in &self.connections {
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);

            if socket.can_recv() {
                let _ = socket.recv(|data| {
                    let n = core.push_from_guest(data);
                    (n, n)
                });
            }
            if !socket.may_recv() {
                core.guest_closed();
            }

            if socket.can_send() {
                let room = (socket.send_capacity() - socket.send_queue()).min(buf.len());
                let n = core.pull_for_guest(&mut buf[..room]);
                if n > 0 {
                    let _ = socket.send_slice(&buf[..n]);
                }
            }

            if core.worker_finished() && socket.may_send() {
                socket.close();
            }
            if !socket.is_open() {
                core.abandon();
                done.push(handle);
            }
        }
        for handle in done {
            self.connections.remove(&handle);
            self.pending_removal.push(handle);
        }
    }
}

fn fresh_listener(sockets: &mut SocketSet<'static>, port: u16) -> SocketHandle {
    let rx = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF]);
    let tx = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF]);
    let mut socket = tcp::Socket::new(rx, tx);
    socket
        .listen(IpListenEndpoint { addr: None, port })
        .expect("a fresh socket can always listen");
    sockets.add(socket)
}

/// A random interface seed. `smoltcp` only uses this to pick IPv4
/// identification fields and initial sequence numbers less predictably; it
/// is not a cryptographic key, so the low bits of the current time are
/// plenty.
fn rand_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}
