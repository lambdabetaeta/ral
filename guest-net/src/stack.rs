//! The core thread: sole owner of `smoltcp`'s [`Interface`] and
//! [`SocketSet`], and of the CONNECT-only accept gate.
//!
//! One listening endpoint — the gateway's own address on port 3128, no
//! `set_any_ip`, no other port. A listener leaving `Listen` *is* the
//! accept: it either gets a tunnel slot and a fresh worker thread, or is
//! aborted. Everything downstream — CONNECT head, policy, dialling,
//! copying — is the worker's; this module never reads application data.

use std::borrow::Cow;
use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant as StdInstant};

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpListenEndpoint};

use exarch::egress::{Egress, Record};

use crate::connect::{self, Refusal};
use crate::device::{self, TunDevice, Wire};
use crate::pipe::{self, CoreSide, ProxyPipe, Wake};
use crate::vet;

/// The one port the guest may ever reach: `10.0.2.2:3128`, per
/// `vm_manager::GUEST_LINK`'s gateway.
const PORT: u16 = 3128;

/// Each socket's own send/receive buffers — `smoltcp`'s TCP window,
/// distinct from [`pipe`]'s 64 KiB rings.
const SOCKET_BUF: usize = 16 * 1024;

/// Idle listeners kept ready on port 3128, replenished as they accept.
/// A smoltcp listener becomes the connection it accepts, so the pool's
/// depth is the backlog, bounding half-open handshakes.
const LISTENER_POOL: usize = 8;

/// How many tunnels may be open or opening at once — caps live workers and
/// dialled origins, unlike [`LISTENER_POOL`]'s handshake bound.
const TUNNEL_CAP: usize = 32;

/// How long a worker waits for a complete CONNECT head before giving up.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);

/// Per-attempt connect timeout handed to [`vet::open`].
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a finished connection may sit half-closed before it is reset.
/// Not a tunnel lifetime — the worker is already gone and no byte can
/// still arrive, so this only lets a normal close finish. Without it a
/// guest holds a [`TUNNEL_CAP`] slot forever by never closing its half.
const FIN_GRACE: Duration = Duration::from_secs(30);

/// Whether a connection's slot can be released: the guest finished closing,
/// or it did not within [`FIN_GRACE`].
fn reap_now(socket_open: bool, worker_done: bool, since_fin: Option<Duration>) -> bool {
    (!socket_open && worker_done) || since_fin.is_some_and(|d| d > FIN_GRACE)
}

const HANDSHAKE_TOO_LARGE: Refusal = Refusal {
    status: 431,
    reason: Cow::Borrowed("CONNECT head exceeded 8 KiB"),
};
const HANDSHAKE_EOF: Refusal = Refusal {
    status: 400,
    reason: Cow::Borrowed("connection closed before a complete CONNECT head"),
};
const HANDSHAKE_TIMEOUT: Refusal = Refusal {
    status: 408,
    reason: Cow::Borrowed("CONNECT head deadline exceeded"),
};

/// Rung by the reader thread and every worker so the core thread's poll
/// loop can park on `poll_delay()` instead of a fixed timer.
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

/// One live accepted connection. `origin` is set once the worker's dial
/// succeeds, so [`Session::stop`] can wake a worker blocked on it;
/// `closing` is stamped when this end sends its FIN.
struct Connection {
    handle: SocketHandle,
    core: CoreSide,
    origin: Arc<Mutex<Option<TcpStream>>>,
    worker: thread::JoinHandle<()>,
    closing: Option<StdInstant>,
}

impl Connection {
    /// Wake this worker wherever it is parked — on either ring, or on the
    /// origin socket.
    fn wake_for_stop(&self) {
        self.core.abandon();
        self.core.guest_closed();
        if let Some(origin) = self.origin.lock().expect("origin poisoned").as_ref() {
            let _ = origin.shutdown(Shutdown::Both);
        }
    }
}

/// A caller's handle on a running session.
pub struct Session<W: Wire> {
    stop: device::Stop<W>,
    core: thread::JoinHandle<()>,
    connections: Arc<Mutex<Vec<Connection>>>,
    failures: Arc<Mutex<Vec<&'static str>>>,
}

impl<W: Wire> Session<W> {
    /// Wake every parked thread — reader, pipes, dialled origins. Returns
    /// promptly; [`Self::join`] is where the session finishes winding down.
    ///
    /// # Panics
    /// If the connections lock is poisoned.
    pub fn stop(&self) {
        self.stop.stop();
        for conn in self
            .connections
            .lock()
            .expect("connections poisoned")
            .iter()
        {
            conn.wake_for_stop();
        }
    }

    /// Wait for every guest-net thread to end. Two bounds, both named
    /// rather than hidden: a worker inside `getaddrinfo` cannot be
    /// interrupted, so this may block for the system resolver's own
    /// timeout; and one already dialling, but not yet far enough to have
    /// published the origin [`Session::stop`] would shut down, blocks for
    /// at most [`DIAL_TIMEOUT`] per vetted address.
    ///
    /// # Errors
    /// Names every worker that panicked, rather than propagating a
    /// detached panic.
    ///
    /// # Panics
    /// If the core thread itself panicked — a bug in this crate, not a
    /// guest-triggerable condition.
    pub fn join(self) -> Result<(), String> {
        self.core.join().expect("guest-net core thread panicked");
        for conn in self
            .connections
            .lock()
            .expect("connections poisoned")
            .drain(..)
        {
            // A connection accepted in the core's last poll never saw
            // `stop`'s wake, so every survivor gets one.
            conn.wake_for_stop();
            if conn.worker.join().is_err() {
                self.failures
                    .lock()
                    .expect("failures poisoned")
                    .push("a guest-net worker panicked");
            }
        }
        self.stop.join();
        let failures = self.failures.lock().expect("failures poisoned");
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

/// Start a session: the reader thread over `wire`, the interface at
/// `config.gateway`, and the core loop on its own thread.
///
/// # Errors
/// Whatever starting either thread reports.
pub fn run<W: Wire>(wire: W, config: crate::Config) -> std::io::Result<Session<W>> {
    let doorbell = Doorbell::new();
    let wake: Wake = {
        let doorbell = doorbell.clone();
        Arc::new(move || doorbell.ring())
    };
    let (device, stop) = device::spawn(wire, Some(wake.clone()))?;

    let connections: Arc<Mutex<Vec<Connection>>> = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let core_connections = connections.clone();
    let core_failures = failures.clone();

    let core = thread::Builder::new()
        .name("guest-net-core".to_string())
        .spawn(move || {
            Core::new(
                device,
                config,
                doorbell,
                wake,
                core_connections,
                core_failures,
            )
            .run();
        })
        .map_err(|e| std::io::Error::other(format!("could not start the guest-net core: {e}")))?;

    Ok(Session {
        stop,
        core,
        connections,
        failures,
    })
}

struct Core<W: Wire> {
    iface: Interface,
    device: TunDevice<W>,
    sockets: SocketSet<'static>,
    egress: Egress,
    dialer: Arc<dyn vet::Dialer>,
    /// Idle listeners on port 3128, always at least [`LISTENER_POOL`] deep.
    pool: Vec<SocketHandle>,
    connections: Arc<Mutex<Vec<Connection>>>,
    failures: Arc<Mutex<Vec<&'static str>>>,
    /// Aborted/closed sockets whose final packet needs one more `poll()` —
    /// reclaimed at the top of the next loop iteration.
    pending_removal: Vec<SocketHandle>,
    wake: Wake,
    doorbell: Arc<Doorbell>,
}

impl<W: Wire> Core<W> {
    fn new(
        mut device: TunDevice<W>,
        config: crate::Config,
        doorbell: Arc<Doorbell>,
        wake: Wake,
        connections: Arc<Mutex<Vec<Connection>>>,
        failures: Arc<Mutex<Vec<&'static str>>>,
    ) -> Self {
        let mut iface_config = IfaceConfig::new(HardwareAddress::Ip);
        iface_config.random_seed = rand_seed();
        let mut iface = Interface::new(iface_config, &mut device, Instant::now());
        iface.update_ip_addrs(|addrs| {
            // 24-bit prefix, matching vm-manager's GuestLink for this link.
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(config.gateway), 24))
                .expect("a fresh interface has room for its one address");
        });

        let mut sockets = SocketSet::new(Vec::new());
        let pool = (0..LISTENER_POOL)
            .map(|_| fresh_listener(&mut sockets))
            .collect();

        Self {
            iface,
            device,
            sockets,
            egress: config.egress,
            dialer: config.dialer,
            pool,
            connections,
            failures,
            pending_removal: Vec::new(),
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

            self.pump_listeners();
            self.pump_connections();

            let wait = self
                .iface
                .poll_delay(now, &self.sockets)
                .map_or(MAX_WAIT, |d| {
                    Duration::from_micros(d.total_micros()).min(MAX_WAIT)
                });
            self.doorbell.wait(wait);
        }
    }

    fn pump_listeners(&mut self) {
        let handles = std::mem::take(&mut self.pool);
        let mut remaining = Vec::with_capacity(handles.len());
        for handle in handles {
            match self.sockets.get::<tcp::Socket>(handle).state() {
                tcp::State::Listen | tcp::State::SynReceived => remaining.push(handle),
                tcp::State::Established => {
                    self.accept(handle);
                    remaining.push(fresh_listener(&mut self.sockets));
                }
                _ => {
                    self.pending_removal.push(handle);
                    remaining.push(fresh_listener(&mut self.sockets));
                }
            }
        }
        self.pool = remaining;
    }

    /// A listener reached `Established`: this is the accept. The only thing
    /// decided here is whether a tunnel slot exists; refusing a host is the
    /// worker's job, once it has read a CONNECT head.
    fn accept(&mut self, handle: SocketHandle) {
        // Two separate lock holds are safe: only this thread ever adds a
        // connection, so the capacity check cannot be raced past.
        let occupied = self.connections.lock().expect("connections poisoned").len();
        if occupied >= TUNNEL_CAP {
            self.sockets.get_mut::<tcp::Socket>(handle).abort();
            self.pending_removal.push(handle);
            return;
        }

        let (core_side, worker_pipe) = pipe::new_pipe(Some(self.wake.clone()));
        let origin = Arc::new(Mutex::new(None));
        let worker_origin = origin.clone();
        let egress = self.egress.clone();
        let dialer = self.dialer.clone();
        if let Ok(worker) = thread::Builder::new()
            .name("guest-net-worker".to_string())
            .spawn(move || worker(worker_pipe, &egress, &*dialer, &worker_origin))
        {
            self.connections
                .lock()
                .expect("connections poisoned")
                .push(Connection {
                    handle,
                    core: core_side,
                    origin,
                    worker,
                    closing: None,
                });
        } else {
            self.sockets.get_mut::<tcp::Socket>(handle).abort();
            self.pending_removal.push(handle);
        }
    }

    fn pump_connections(&mut self) {
        let mut buf = [0u8; 4096];
        let mut reaped = Vec::new();
        let mut connections = self.connections.lock().expect("connections poisoned");
        let mut i = 0;
        while i < connections.len() {
            let handle = connections[i].handle;
            let reap = {
                let conn = &mut connections[i];
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);

                if socket.can_recv() {
                    let _ = socket.recv(|data| {
                        let n = conn.core.push_from_guest(data);
                        (n, n)
                    });
                }
                if !socket.may_recv() {
                    conn.core.guest_closed();
                }

                if socket.can_send() {
                    let room = (socket.send_capacity() - socket.send_queue()).min(buf.len());
                    let n = conn.core.pull_for_guest(&mut buf[..room]);
                    if n > 0 {
                        let _ = socket.send_slice(&buf[..n]);
                    }
                }

                if conn.core.worker_finished() {
                    if socket.may_send() {
                        socket.close();
                    }
                    conn.closing.get_or_insert_with(StdInstant::now);
                }
                reap_now(
                    socket.is_open(),
                    conn.worker.is_finished(),
                    conn.closing.map(|t| t.elapsed()),
                )
            };
            if reap {
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);
                if socket.is_open() {
                    // The grace expired: reset, so smoltcp frees it rather
                    // than waiting on a FIN the guest is withholding.
                    socket.abort();
                }
                self.pending_removal.push(handle);
                reaped.push(connections.remove(i));
            } else {
                i += 1;
            }
        }
        drop(connections);
        for conn in reaped {
            if conn.worker.join().is_err() {
                self.failures
                    .lock()
                    .expect("failures poisoned")
                    .push("a guest-net worker panicked");
            }
        }
    }
}

/// The longest the core thread ever parks between polls when nothing has
/// rung the doorbell.
const MAX_WAIT: Duration = Duration::from_millis(200);

fn fresh_listener(sockets: &mut SocketSet<'static>) -> SocketHandle {
    let rx = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF]);
    let tx = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF]);
    let mut socket = tcp::Socket::new(rx, tx);
    socket
        .listen(IpListenEndpoint {
            addr: None,
            port: PORT,
        })
        .expect("a fresh socket can always listen");
    sockets.add(socket)
}

/// `smoltcp` uses this only to de-predict IPv4 id fields and initial
/// sequence numbers; the low bits of the current time are plenty.
fn rand_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Record the refusal, then write its HTTP status line — the only path by
/// which the reason reaches whoever asked.
fn refuse(pipe: &mut ProxyPipe, egress: &Egress, host: &str, r: &Refusal) {
    let _ = egress.audit.record(Record::Tunnel {
        host,
        addr: None,
        allowed: false,
        note: Some(&r.reason),
        up: None,
        down: None,
    });
    let _ = write!(pipe, "HTTP/1.1 {} {}\r\n\r\n", r.status, r.reason);
}

/// The CONNECT door, one thread per accepted connection: read the head,
/// decode, check policy, dial — and only once all agree, tunnel bytes both
/// ways without parsing them.
fn worker(
    mut pipe: ProxyPipe,
    egress: &Egress,
    dialer: &dyn vet::Dialer,
    origin_slot: &Mutex<Option<TcpStream>>,
) {
    let deadline = StdInstant::now() + HANDSHAKE_DEADLINE;
    let mut buf = [0u8; connect::HEAD_MAX];
    let mut len = 0usize;
    let connect = loop {
        match connect::decode(&buf[..len]) {
            Ok(Some(c)) => break c,
            Ok(None) => {}
            Err(r) => return refuse(&mut pipe, egress, "", &r),
        }
        if len == buf.len() {
            return refuse(&mut pipe, egress, "", &HANDSHAKE_TOO_LARGE);
        }
        match pipe.read_deadline(&mut buf[len..], deadline) {
            Ok(0) => return refuse(&mut pipe, egress, "", &HANDSHAKE_EOF),
            Ok(n) => len += n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return refuse(&mut pipe, egress, "", &HANDSHAKE_TIMEOUT);
            }
            Err(_) => return,
        }
    };

    let host = connect.host.as_str().to_string();
    if !egress.policy.admits(&connect.host) {
        return refuse(
            &mut pipe,
            egress,
            &host,
            &Refusal {
                status: 403,
                reason: Cow::Owned(exarch::net_policy::refusal(&host)),
            },
        );
    }

    let (origin, addr) = match vet::open(&connect.host, dialer, DIAL_TIMEOUT) {
        Ok(ok) => ok,
        Err(r) => return refuse(&mut pipe, egress, &host, &r),
    };
    let Ok(watch) = origin.try_clone() else {
        return;
    };
    *origin_slot.lock().expect("origin slot poisoned") = Some(watch);

    if egress
        .audit
        .record(Record::Tunnel {
            host: &host,
            addr: Some(addr),
            allowed: true,
            note: None,
            up: None,
            down: None,
        })
        .is_err()
    {
        return; // an unauditable proxy does not proxy
    }
    if pipe
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .is_err()
    {
        return;
    }

    let (mut reader, mut writer) = pipe.split();
    let Ok(mut origin_write) = origin.try_clone() else {
        return;
    };
    let mut origin_read = origin;
    let up = AtomicU64::new(0);
    let down = AtomicU64::new(0);
    thread::scope(|scope| {
        scope.spawn(|| {
            let n = std::io::copy(&mut reader, &mut origin_write).unwrap_or(0);
            up.store(n, Ordering::Relaxed);
            let _ = origin_write.shutdown(Shutdown::Both);
        });
        scope.spawn(|| {
            let n = std::io::copy(&mut origin_read, &mut writer).unwrap_or(0);
            down.store(n, Ordering::Relaxed);
            let _ = origin_read.shutdown(Shutdown::Both);
        });
    });

    let _ = egress.audit.record(Record::Tunnel {
        host: &host,
        addr: Some(addr),
        allowed: true,
        note: Some("closed"),
        up: Some(up.load(Ordering::Relaxed)),
        down: Some(down.load(Ordering::Relaxed)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_is_reaped_once_the_guest_finishes_closing() {
        assert!(reap_now(false, true, Some(Duration::ZERO)));
    }

    #[test]
    fn a_live_connection_is_never_reaped() {
        assert!(!reap_now(true, false, None));
    }

    #[test]
    fn a_half_closed_connection_is_left_alone_inside_the_grace() {
        assert!(!reap_now(true, true, Some(FIN_GRACE / 2)));
    }

    /// The guest cannot hold a tunnel slot by never closing its half.
    #[test]
    fn a_half_closed_connection_is_reaped_once_the_grace_expires() {
        assert!(reap_now(
            true,
            true,
            Some(FIN_GRACE + Duration::from_secs(1))
        ));
    }
}
