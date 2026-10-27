//! The core alone owns smoltcp and the gateway's single CONNECT endpoint.
//! Workers decode, vet, audit and copy; the core never reads application data.

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
use smoltcp::wire::{HardwareAddress, IpCidr, IpListenEndpoint};

use guest_abi::link::{LINK, PROXY_PORT};
use ral_core::egress::{NetPolicy, refusal};
use ral_core::protocol::channel::WireStream;
use ral_core::sync::{CondvarExt as _, LockExt as _};

use crate::connect;
use crate::device::{self, TunDevice};
use crate::ledger::{AuditLog, Record};
use crate::pipe::{self, CoreSide, ProxyPipe, Wake};
use crate::refusal::Refusal;
use crate::vet;

#[derive(Clone)]
pub struct Config {
    pub policy: Arc<NetPolicy>,
    pub audit: AuditLog,
    pub dialer: Arc<dyn vet::Dialer>,
}

const SOCKET_BUF: usize = 16 * 1024;
const WORKER_PANICKED: &str = "a netgate worker panicked";

/// A listener becomes its accepted connection; the pool bounds half-open handshakes.
const LISTENER_POOL: usize = 8;

/// Bounds both opening and open tunnels, independently of half-open handshakes.
const TUNNEL_CAP: usize = 32;

const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);

const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Retirement grace starts after worker completion and output drain, never during tunnelling.
const FIN_GRACE: Duration = Duration::from_secs(30);

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

/// Publish the origin for stop; start retirement grace only after worker and output end.
struct Connection {
    handle: SocketHandle,
    core: CoreSide,
    origin: Arc<Mutex<Option<TcpStream>>>,
    worker: thread::JoinHandle<()>,
    finished_at: Option<StdInstant>,
}

impl Connection {
    fn wake_for_stop(&self) {
        self.core.abandon();
        self.core.guest_closed();
        if let Some(origin) = self.origin.lock_ignore_poison().as_ref() {
            let _ = origin.shutdown(Shutdown::Both);
        }
    }
}

pub struct Session {
    stop: device::Stop,
    core: thread::JoinHandle<()>,
    connections: Arc<Mutex<Vec<Connection>>>,
    failures: Arc<Mutex<Vec<&'static str>>>,
}

impl Session {
    pub fn stop(&self) {
        self.stop.stop();
        for conn in self.connections.lock_ignore_poison().iter() {
            conn.wake_for_stop();
        }
    }

    /// # Errors
    /// Reports worker panics; joining waits for resolver and per-address dial timeouts.
    ///
    /// # Panics
    /// If the core or reader thread panicked.
    pub fn join(self) -> Result<(), String> {
        self.core.join().expect("netgate core thread panicked");
        for conn in self.connections.lock_ignore_poison().drain(..) {
            // A connection accepted in the last poll may have missed the stop wake.
            conn.wake_for_stop();
            if conn.worker.join().is_err() {
                self.failures.lock_ignore_poison().push(WORKER_PANICKED);
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

/// # Errors
/// Reports wire cloning or thread startup failure.
pub fn run(wire: WireStream, config: Config) -> std::io::Result<Session> {
    let doorbell = Doorbell::new();
    let wake: Wake = {
        let doorbell = doorbell.clone();
        Arc::new(move || doorbell.ring())
    };
    let (device, stop) = device::spawn(wire, wake.clone())?;

    let connections: Arc<Mutex<Vec<Connection>>> = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let core_connections = connections.clone();
    let core_failures = failures.clone();

    let core = thread::Builder::new()
        .name("netgate-core".to_string())
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
        .map_err(|e| std::io::Error::other(format!("could not start the netgate core: {e}")))?;

    Ok(Session {
        stop,
        core,
        connections,
        failures,
    })
}

struct Core {
    iface: Interface,
    device: TunDevice,
    sockets: SocketSet<'static>,
    config: Config,
    pool: Vec<SocketHandle>,
    connections: Arc<Mutex<Vec<Connection>>>,
    failures: Arc<Mutex<Vec<&'static str>>>,
    /// A final packet needs one more poll before these sockets can be removed.
    pending_removal: Vec<SocketHandle>,
    wake: Wake,
    doorbell: Arc<Doorbell>,
}

impl Core {
    fn new(
        mut device: TunDevice,
        config: Config,
        doorbell: Arc<Doorbell>,
        wake: Wake,
        connections: Arc<Mutex<Vec<Connection>>>,
        failures: Arc<Mutex<Vec<&'static str>>>,
    ) -> Self {
        let mut iface_config = IfaceConfig::new(HardwareAddress::Ip);
        iface_config.random_seed = rand_seed();
        let mut iface = Interface::new(iface_config, &mut device, Instant::now());
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(LINK.gateway.into(), LINK.prefix))
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
            config,
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

    fn accept(&mut self, handle: SocketHandle) {
        // Only this thread adds connections, so the separate capacity check cannot race.
        let occupied = self.connections.lock_ignore_poison().len();
        if occupied >= TUNNEL_CAP {
            self.sockets.get_mut::<tcp::Socket>(handle).abort();
            self.pending_removal.push(handle);
            return;
        }

        let (core_side, worker_pipe) = pipe::new_pipe(self.wake.clone());
        let origin = Arc::new(Mutex::new(None));
        let worker_origin = origin.clone();
        let config = self.config.clone();
        if let Ok(worker) = thread::Builder::new()
            .name("netgate-worker".to_string())
            .spawn(move || worker(worker_pipe, &config, &worker_origin))
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
                self.failures.lock_ignore_poison().push(WORKER_PANICKED);
            }
        }
    }
}

const MAX_WAIT: Duration = Duration::from_millis(200);

fn fresh_listener(sockets: &mut SocketSet<'static>) -> SocketHandle {
    let rx = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF]);
    let tx = tcp::SocketBuffer::new(vec![0u8; SOCKET_BUF]);
    let mut socket = tcp::Socket::new(rx, tx);
    socket
        .listen(IpListenEndpoint {
            addr: None,
            port: PROXY_PORT,
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

/// Ledger the refusal before returning its HTTP status.
fn refuse(pipe: &mut ProxyPipe, audit: &AuditLog, host: &str, r: &Refusal) {
    let _ = audit.record(Record::Tunnel {
        host,
        addr: None,
        allowed: false,
        note: Some(&r.reason),
        up: None,
        down: None,
    });
    let _ = write!(pipe, "HTTP/1.1 {} {}\r\n\r\n", r.status, r.reason);
}

/// Audit admission before 200; tunnel bytes remain opaque.
fn worker(mut pipe: ProxyPipe, config: &Config, origin_slot: &Mutex<Option<TcpStream>>) {
    let deadline = StdInstant::now() + HANDSHAKE_DEADLINE;
    let mut buf = [0u8; connect::HEAD_MAX];
    let mut len = 0usize;
    let connect = loop {
        match connect::decode(&buf[..len]) {
            Ok(Some(c)) => break c,
            Ok(None) => {}
            Err(r) => return refuse(&mut pipe, &config.audit, "", &r),
        }
        if len == buf.len() {
            return refuse(&mut pipe, &config.audit, "", &HANDSHAKE_TOO_LARGE);
        }
        match pipe.read_deadline(&mut buf[len..], deadline) {
            Ok(0) => return refuse(&mut pipe, &config.audit, "", &HANDSHAKE_EOF),
            Ok(n) => len += n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return refuse(&mut pipe, &config.audit, "", &HANDSHAKE_TIMEOUT);
            }
            Err(_) => return,
        }
    };

    let host = connect.host.as_str().to_string();
    if !config.policy.admits(&connect.host) {
        return refuse(
            &mut pipe,
            &config.audit,
            &host,
            &Refusal {
                status: 403,
                reason: Cow::Owned(refusal(&host)),
            },
        );
    }

    let (origin, addr) = match vet::open(&connect.host, &*config.dialer, DIAL_TIMEOUT) {
        Ok(ok) => ok,
        Err(r) => return refuse(&mut pipe, &config.audit, &host, &r),
    };
    let Ok(watch) = origin.try_clone() else {
        return;
    };
    *origin_slot.lock_ignore_poison() = Some(watch);

    if config
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

    let _ = config.audit.record(Record::Tunnel {
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
        use smoltcp::wire::IpAddress;
        use std::net::Ipv4Addr;
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc::{self, Receiver, Sender};

        const WAIT: Duration = Duration::from_secs(5);
        const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

        struct Tunnel {
            host: SocketHandle,
            guest: SocketHandle,
            uploaded: Receiver<Vec<u8>>,
            finish: Sender<()>,
        }

        struct Harness {
            core: Core,
            guest_device: TunDevice,
            guest_iface: Interface,
            guest_sockets: SocketSet<'static>,
            stops: Vec<device::Stop>,
            now: StdInstant,
        }

        impl Harness {
            fn new() -> Self {
                let (host, guest) = UnixStream::pair().unwrap();
                let (host_device, host_stop) = device::spawn(host, Arc::new(|| {})).unwrap();
                let (mut guest_device, guest_stop) = device::spawn(guest, Arc::new(|| {})).unwrap();
                let mut config = IfaceConfig::new(HardwareAddress::Ip);
                config.random_seed = 1;
                let mut guest_iface = Interface::new(config, &mut guest_device, Instant::now());
                guest_iface.update_ip_addrs(|addrs| {
                    addrs.push(IpCidr::new(IpAddress::Ipv4(GUEST), 24)).unwrap();
                });
                let core = Core::new(
                    host_device,
                    Config {
                        policy: Arc::new(NetPolicy::for_test(&["example.com", "a.example"])),
                        audit: AuditLog::in_scratch(),
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
                        (IpAddress::Ipv4(LINK.gateway), PROXY_PORT),
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
                let (core, pipe) = pipe::new_pipe(Arc::new(|| {}));
                let (uploaded_tx, uploaded) = mpsc::channel();
                let (finish, finish_rx) = mpsc::channel();
                let size = expected.len();
                let worker = thread::spawn(move || {
                    let (reader, writer) = pipe.split();
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
