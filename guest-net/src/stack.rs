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
use ral_core::sync::{CondvarExt as _, LockExt as _};

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
fn reap_now(socket_open: bool, worker_done: bool, since_finished: Option<Duration>) -> bool {
    worker_done && (!socket_open || since_finished.is_some_and(|d| d > FIN_GRACE))
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
        *self.dirty.lock_ignore_poison() = true;
        self.cvar.notify_one();
    }

    fn wait(&self, timeout: Duration) {
        let mut dirty = self.dirty.lock_ignore_poison();
        if !*dirty {
            dirty = self.cvar.wait_timeout_ignore_poison(dirty, timeout).0;
        }
        *dirty = false;
    }
}

/// One live accepted connection. `origin` is set once the worker's dial
/// succeeds, so [`Session::stop`] can wake a worker blocked on it;
/// `finished_at` starts retirement grace only after worker and output end.
struct Connection {
    handle: SocketHandle,
    core: CoreSide,
    origin: Arc<Mutex<Option<TcpStream>>>,
    worker: thread::JoinHandle<()>,
    finished_at: Option<StdInstant>,
}

impl Connection {
    /// Wake this worker wherever it is parked — on either ring, or on the
    /// origin socket.
    fn wake_for_stop(&self) {
        self.core.abandon();
        self.core.guest_closed();
        if let Some(origin) = self.origin.lock_ignore_poison().as_ref() {
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
        for conn in self.connections.lock_ignore_poison().iter() {
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
        for conn in self.connections.lock_ignore_poison().drain(..) {
            // A connection accepted in the core's last poll never saw
            // `stop`'s wake, so every survivor gets one.
            conn.wake_for_stop();
            if conn.worker.join().is_err() {
                self.failures
                    .lock_ignore_poison()
                    .push("a guest-net worker panicked");
            }
        }
        self.stop.join();
        let failures = self.failures.lock_ignore_poison();
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
        let occupied = self.connections.lock_ignore_poison().len();
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
            self.connections.lock_ignore_poison().push(Connection {
                handle,
                core: core_side,
                origin,
                worker,
                finished_at: None,
            });
        } else {
            self.sockets.get_mut::<tcp::Socket>(handle).abort();
            self.pending_removal.push(handle);
        }
    }

    fn pump_connections(&mut self) {
        self.pump_connections_at(StdInstant::now());
    }

    fn pump_connections_at(&mut self, now: StdInstant) {
        let mut buf = [0u8; 4096];
        let mut reaped = Vec::new();
        let mut connections = self.connections.lock_ignore_poison();
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

                // TIME-WAIT may still have upload bytes queued for the origin.
                if socket.state() == tcp::State::Closed {
                    conn.wake_for_stop();
                }
                let worker_done = conn.worker.is_finished();
                if conn.core.output_finished() {
                    if socket.may_send() {
                        socket.close();
                    }
                    if worker_done {
                        conn.finished_at.get_or_insert(now);
                    }
                }
                reap_now(
                    socket.is_open(),
                    worker_done,
                    conn.finished_at.map(|t| now.duration_since(t)),
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
                    .lock_ignore_poison()
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
    *origin_slot.lock_ignore_poison() = Some(watch);

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

    let abort = pipe.abort_handle();
    let (mut reader, mut writer) = pipe.split();
    let Ok(mut origin_write) = origin.try_clone() else {
        return;
    };
    let mut origin_read = origin;
    let up = AtomicU64::new(0);
    let down = AtomicU64::new(0);
    thread::scope(|scope| {
        let up = &up;
        let down = &down;
        let upload_abort = abort.clone();
        scope.spawn(move || {
            if let Ok(n) = std::io::copy(&mut reader, &mut origin_write) {
                up.store(n, Ordering::Relaxed);
                let _ = origin_write.shutdown(Shutdown::Write);
            } else {
                upload_abort.abort();
                let _ = origin_write.shutdown(Shutdown::Both);
            }
        });
        scope.spawn(move || {
            if let Ok(n) = std::io::copy(&mut origin_read, &mut writer) {
                down.store(n, Ordering::Relaxed);
            } else {
                abort.abort();
                let _ = origin_read.shutdown(Shutdown::Both);
            }
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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn a_connection_is_reaped_once_the_guest_finishes_closing() {
        assert!(reap_now(false, true, Some(Duration::ZERO)));
    }

    #[test]
    fn a_live_connection_is_never_reaped() {
        assert!(!reap_now(true, false, None));
        assert!(!reap_now(false, false, None));
        assert!(!reap_now(
            true,
            false,
            Some(FIN_GRACE + Duration::from_secs(1))
        ));
        assert!(!reap_now(
            false,
            false,
            Some(FIN_GRACE + Duration::from_secs(1))
        ));
    }

    #[test]
    fn a_half_closed_connection_is_left_alone_inside_the_grace() {
        assert!(!reap_now(true, true, Some(FIN_GRACE / 2)));
        assert!(!reap_now(true, true, Some(FIN_GRACE)));
    }

    #[cfg(unix)]
    mod pump {
        use super::*;
        use std::net::Ipv4Addr;
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc::{self, Receiver, Sender};

        const WAIT: Duration = Duration::from_secs(5);
        const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
        const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

        struct Tunnel {
            host: SocketHandle,
            guest: SocketHandle,
            uploaded: Receiver<Vec<u8>>,
            finish: Sender<()>,
        }

        struct Harness {
            core: Core<UnixStream>,
            guest_device: TunDevice<UnixStream>,
            guest_iface: Interface,
            guest_sockets: SocketSet<'static>,
            stops: Vec<device::Stop<UnixStream>>,
            now: StdInstant,
        }

        impl Harness {
            fn new() -> Self {
                let (host, guest) = UnixStream::pair().unwrap();
                let (host_device, host_stop) = device::spawn(host, None).unwrap();
                let (mut guest_device, guest_stop) = device::spawn(guest, None).unwrap();
                let mut config = IfaceConfig::new(HardwareAddress::Ip);
                config.random_seed = 1;
                let mut guest_iface = Interface::new(config, &mut guest_device, Instant::now());
                guest_iface.update_ip_addrs(|addrs| {
                    addrs.push(IpCidr::new(IpAddress::Ipv4(GUEST), 24)).unwrap();
                });
                let core = Core::new(
                    host_device,
                    crate::Config {
                        gateway: GATEWAY,
                        egress: Egress::for_test(),
                        dialer: Arc::new(vet::System),
                    },
                    Doorbell::new(),
                    Arc::new(|| {}),
                    Arc::new(Mutex::new(Vec::new())),
                    Arc::new(Mutex::new(Vec::new())),
                );
                Self {
                    core,
                    guest_device,
                    guest_iface,
                    guest_sockets: SocketSet::new(Vec::new()),
                    stops: vec![host_stop, guest_stop],
                    now: StdInstant::now(),
                }
            }

            fn step(&mut self) {
                self.guest_iface.poll(
                    Instant::now(),
                    &mut self.guest_device,
                    &mut self.guest_sockets,
                );
                self.core.iface.poll(
                    Instant::now(),
                    &mut self.core.device,
                    &mut self.core.sockets,
                );
                for handle in self.core.pending_removal.drain(..) {
                    self.core.sockets.remove(handle);
                }
                self.core.pump_connections_at(self.now);
            }

            fn until(&mut self, mut done: impl FnMut(&mut Self) -> bool) {
                let deadline = StdInstant::now() + WAIT;
                loop {
                    self.step();
                    if done(self) {
                        return;
                    }
                    assert!(StdInstant::now() < deadline, "pump fixture timed out");
                    thread::sleep(Duration::from_millis(1));
                }
            }

            // Only the worker is controlled: handshake, rings, FIN and reaping are real.
            fn tunnel(&mut self, port: u16, expected: &[u8]) -> Tunnel {
                let guest = self.guest_sockets.add(tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0; SOCKET_BUF]),
                    tcp::SocketBuffer::new(vec![0; SOCKET_BUF]),
                ));
                self.guest_sockets
                    .get_mut::<tcp::Socket>(guest)
                    .connect(
                        self.guest_iface.context(),
                        (IpAddress::Ipv4(GATEWAY), PORT),
                        port,
                    )
                    .unwrap();
                self.until(|h| {
                    h.core.pool.iter().any(|&handle| {
                        h.core.sockets.get::<tcp::Socket>(handle).state() == tcp::State::Established
                    })
                });
                let index = self
                    .core
                    .pool
                    .iter()
                    .position(|&handle| {
                        self.core.sockets.get::<tcp::Socket>(handle).state()
                            == tcp::State::Established
                    })
                    .unwrap();
                let host = self.core.pool.swap_remove(index);
                self.core.pool.push(fresh_listener(&mut self.core.sockets));
                let (core, pipe) = pipe::new_pipe(None);
                let (uploaded_tx, uploaded) = mpsc::channel();
                let (finish, finish_rx) = mpsc::channel();
                let size = expected.len();
                let worker = thread::spawn(move || {
                    let (mut reader, writer) = pipe.split();
                    drop(writer);
                    let deadline = StdInstant::now() + WAIT;
                    let mut bytes = vec![0; size];
                    let mut offset = 0;
                    while offset < size {
                        let Ok(n) = reader.read_deadline(&mut bytes[offset..], deadline) else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        offset += n;
                    }
                    if uploaded_tx.send(bytes).is_ok() {
                        let _ = finish_rx.recv_timeout(WAIT);
                    }
                });
                self.core.connections.lock_ignore_poison().push(Connection {
                    handle: host,
                    core,
                    origin: Arc::new(Mutex::new(None)),
                    worker,
                    finished_at: None,
                });
                self.until(|h| {
                    h.guest_sockets.get::<tcp::Socket>(guest).state() == tcp::State::CloseWait
                });
                Tunnel {
                    host,
                    guest,
                    uploaded,
                    finish,
                }
            }

            fn upload(&mut self, tunnel: &Tunnel, bytes: &[u8]) {
                assert_eq!(
                    self.guest_sockets
                        .get_mut::<tcp::Socket>(tunnel.guest)
                        .send_slice(bytes)
                        .unwrap(),
                    bytes.len(),
                );
                self.until(|_| match tunnel.uploaded.try_recv() {
                    Ok(received) => {
                        assert_eq!(received, bytes);
                        true
                    }
                    Err(mpsc::TryRecvError::Empty) => false,
                    Err(mpsc::TryRecvError::Disconnected) => panic!("upload worker ended early"),
                });
            }
        }

        impl Drop for Harness {
            fn drop(&mut self) {
                let connections = std::mem::take(&mut *self.core.connections.lock_ignore_poison());
                for conn in &connections {
                    conn.wake_for_stop();
                }
                let deadline = StdInstant::now() + WAIT + WAIT;
                for conn in connections {
                    while !conn.worker.is_finished() && StdInstant::now() < deadline {
                        thread::sleep(Duration::from_millis(1));
                    }
                    assert!(
                        conn.worker.is_finished(),
                        "pump fixture worker did not stop"
                    );
                    conn.worker.join().unwrap();
                }
                let stops = std::mem::take(&mut self.stops);
                for stop in &stops {
                    stop.stop();
                }
                let readers = thread::spawn(move || {
                    for stop in stops {
                        stop.join();
                    }
                });
                let deadline = StdInstant::now() + WAIT;
                while !readers.is_finished() && StdInstant::now() < deadline {
                    thread::sleep(Duration::from_millis(1));
                }
                assert!(readers.is_finished(), "pump fixture readers did not stop");
                readers.join().unwrap();
            }
        }

        #[test]
        fn output_fin_does_not_age_a_live_worker_and_completion_starts_fresh_grace() {
            let mut h = Harness::new();
            let start = h.now;
            let first = h.tunnel(42000, b"upload after FIN");
            {
                let connections = h.core.connections.lock_ignore_poison();
                assert!(connections[0].core.output_finished());
                assert!(!connections[0].worker.is_finished());
                assert_eq!(connections[0].finished_at, None);
            }

            h.now = start + FIN_GRACE + Duration::from_secs(1);
            h.step();
            assert_eq!(h.core.connections.lock_ignore_poison().len(), 1);
            assert_eq!(h.core.connections.lock_ignore_poison()[0].finished_at, None);
            h.upload(&first, b"upload after FIN");

            // Also exercise the elapsed-grace guard with a still-live worker.
            h.core.connections.lock_ignore_poison()[0].finished_at = Some(start);
            h.step();
            assert_eq!(h.core.connections.lock_ignore_poison().len(), 1);
            assert!(h.core.sockets.get::<tcp::Socket>(first.host).is_open());
            h.core.connections.lock_ignore_poison()[0].finished_at = None;

            let second = h.tunnel(42001, b"another tunnel progresses");
            h.upload(&second, b"another tunnel progresses");
            assert_eq!(h.core.connections.lock_ignore_poison().len(), 2);
            first.finish.send(()).unwrap();
            second.finish.send(()).unwrap();
            let completed = h.now;
            h.until(|h| {
                let connections = h.core.connections.lock_ignore_poison();
                connections.len() == 2
                    && connections
                        .iter()
                        .all(|conn| conn.finished_at == Some(completed))
            });

            for elapsed in [Duration::ZERO, FIN_GRACE / 2, FIN_GRACE] {
                h.now = completed + elapsed;
                h.core.pump_connections_at(h.now);
                assert_eq!(h.core.connections.lock_ignore_poison().len(), 2);
                assert_eq!(h.core.pending_removal, []);
            }
            h.now = completed + FIN_GRACE + Duration::from_nanos(1);
            h.core.pump_connections_at(h.now);
            assert!(h.core.connections.lock_ignore_poison().is_empty());
            assert_eq!(h.core.pending_removal.len(), 2);
            for tunnel in [&first, &second] {
                assert!(h.core.pending_removal.contains(&tunnel.host));
                assert_eq!(
                    h.core.sockets.get::<tcp::Socket>(tunnel.host).state(),
                    tcp::State::Closed
                );
            }
            assert!(h.core.failures.lock_ignore_poison().is_empty());

            let reused = h.tunnel(42002, b"reused slot");
            h.upload(&reused, b"reused slot");
            assert_eq!(h.core.connections.lock_ignore_poison().len(), 1);
        }
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
