//! The factory owns the machine until the host has joined every agent.

use std::io::Write;
use std::sync::{Arc, Mutex};

use exarch::agent::{AgentHandle, EngineLost, Factory, RootConfig, RootEngine};
use exarch::bus::BusReceiver;
use exarch::host::Host;
use exarch::provider::Provider;
use ral_core::authority::policy::Capabilities;
use ral_core::own::OwnDir;
use ral_core::path::{GuestPath, shown};
use vm_manager::{Booted, GuestConsole, Machine};

use ral_core::protocol::channel::WireStream;
use ral_core::sync::LockExt;

struct MachineFactory(Mutex<Box<dyn Machine>>);

impl MachineFactory {
    fn console(&self) -> GuestConsole {
        self.0.lock_ignore_poison().console()
    }
}

impl Factory for MachineFactory {
    fn engine(&self) -> Result<WireStream, String> {
        self.0
            .lock_ignore_poison()
            .dial_engine()
            .map(WireStream::from)
            .map_err(|e| format!("could not dial the guest's engine factory: {e}"))
    }
}

pub(crate) struct Guest {
    factory: Arc<MachineFactory>,
    host: Host,
    net: netgate::Session,
}

impl Guest {
    pub(crate) fn start(
        booted: Booted,
        mut config: RootConfig,
        ceiling: Capabilities,
        provider: Arc<Provider>,
        net: netgate::Config,
    ) -> Result<Self, String> {
        let Booted { machine, wires } = booted;
        let root_engine = RootEngine::guest(
            wires.control.into(),
            GuestPath::from_static(guest_abi::layout::WORK),
            GuestPath::from_static(guest_abi::layout::SCRATCH),
            ceiling,
        )?;
        let factory = Arc::new(MachineFactory(Mutex::new(machine)));
        config.factory = Some(factory.clone());
        let host = Host::found(config, root_engine, provider)
            .map_err(|why| start_failed(&factory, &why))?;
        let net = netgate::run(wires.net.into(), net)
            .map_err(|why| format!("could not start guest networking: {why}"))?;
        Ok(Self { factory, host, net })
    }

    pub(crate) fn handle(&self) -> AgentHandle {
        self.host.roots()[0].clone()
    }

    pub(crate) fn feed(&self) -> &BusReceiver {
        self.host.feed()
    }

    pub(crate) fn run_dir(&self) -> &OwnDir {
        self.host.run_dir()
    }

    pub(crate) fn capture(&self, lost: &EngineLost) {
        capture(self.run_dir(), &lost.logged(), &self.factory.console());
    }

    pub(crate) fn end(self) -> Result<(), String> {
        let Self { factory, host, net } = self;
        // Agent joins precede machine recovery, net join, and shutdown.
        let failures = host.end();
        let machine = Arc::into_inner(factory)
            .expect("host teardown must release every guest factory owner")
            .0
            .into_inner_ignore_poison();
        net.stop();
        let net_end = net.join();
        machine
            .shutdown()
            .map_err(|why| {
                format!(
                    "the machine did not stop cleanly: {}",
                    why.worded(|path| shown(path))
                )
            })
            .and(net_end)
            .and_then(|()| {
                if failures.is_empty() {
                    Ok(())
                } else {
                    Err(failures.join("\n"))
                }
            })
    }
}

fn start_failed(factory: &MachineFactory, why: &std::io::Error) -> String {
    let Some(lost) = why
        .get_ref()
        .and_then(|source| source.downcast_ref::<exarch::agent::EngineLost>())
    else {
        return format!("the assistant could not be set up: {why}");
    };
    capture(lost.log_dir(), &lost.logged(), &factory.console());
    lost.to_string()
}

const FILE: &str = "engine.log";

/// Capture is best-effort: failure must not hide the engine's original error.
fn capture(run_dir: &OwnDir, why: &str, console: &GuestConsole) -> Option<()> {
    let mut file = run_dir.append(FILE).ok()?;
    file.write_all(console.report(why, |p| shown(p)).as_bytes())
        .ok()?;
    file.flush().ok()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use exarch::bus::Signal;
    use exarch::provider::scripted::{Reply, Script};
    use exarch::record::{Forensic, JobId, Record};
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[cfg(windows)]
    use std::net::TcpStream as GuestEnd;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream as GuestEnd;

    #[cfg(unix)]
    fn local_pair() -> std::io::Result<(GuestEnd, vm_manager::Socket)> {
        let (guest, host) = ral_core::process::cloexec_socketpair()?;
        Ok((guest, std::os::fd::OwnedFd::from(host)))
    }

    /// Windows has no socketpair; use an ephemeral loopback connection.
    #[cfg(windows)]
    fn local_pair() -> std::io::Result<(GuestEnd, vm_manager::Socket)> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let host = std::net::TcpStream::connect(listener.local_addr()?)?;
        let (guest, _peer) = listener.accept()?;
        Ok((guest, std::os::windows::io::OwnedSocket::from(host)))
    }

    struct FakeMachine {
        guest_ends: Arc<Mutex<Vec<GuestEnd>>>,
        shutdown: Arc<AtomicBool>,
    }

    impl vm_manager::Machine for FakeMachine {
        fn dial_engine(&self) -> std::io::Result<vm_manager::Socket> {
            let (guest, host) = local_pair()?;
            self.guest_ends.lock_ignore_poison().push(guest);
            Ok(host)
        }
        fn connect_guest(&self, _port: u32) -> std::io::Result<vm_manager::Socket> {
            unimplemented!("not exercised by these tests")
        }
        fn console(&self) -> vm_manager::GuestConsole {
            vm_manager::GuestConsole::Unkept
        }
        fn shutdown(self: Box<Self>) -> Result<(), vm_manager::Error> {
            assert!(!self.shutdown.swap(true, Ordering::SeqCst));
            Ok(())
        }
    }

    fn machine_factory() -> (Arc<MachineFactory>, Arc<AtomicBool>) {
        let shutdown = Arc::new(AtomicBool::new(false));
        let factory = Arc::new(MachineFactory(Mutex::new(Box::new(FakeMachine {
            guest_ends: Arc::default(),
            shutdown: shutdown.clone(),
        }))));
        (factory, shutdown)
    }

    fn host_with_factory(factory: &Arc<MachineFactory>, run_dir: &OwnDir) -> Host {
        crate::test_fixture::scripted_host(
            run_dir,
            Some(factory.clone()),
            Script::new().then(Reply::text("done")),
        )
    }

    #[test]
    fn host_end_joins_the_root_and_makes_the_machine_factory_unique() {
        let run_dir = OwnDir::for_test("guest-factory-join").expect("run directory");
        let (factory, shutdown) = machine_factory();
        let host = host_with_factory(&factory, &run_dir);
        let root = host.roots()[0].clone();
        assert!(root.is_live());
        assert!(Arc::strong_count(&factory) > 1);

        assert_eq!(host.end(), Vec::<String>::new());
        assert!(!root.is_live());
        let machine = Arc::into_inner(factory)
            .expect("joined agents and signal forwarding release every factory owner")
            .0
            .into_inner_ignore_poison();
        machine.shutdown().expect("recovered machine shuts down");
        assert!(shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn guest_exposes_the_hosts_jobs_and_recovers_the_machine_on_end() {
        let run_dir = OwnDir::for_test("guest-host-migration").expect("run directory");
        let (factory, shutdown) = machine_factory();
        let weak_factory = Arc::downgrade(&factory);
        let host = host_with_factory(&factory, &run_dir);
        let (net_peer, net_wire) = local_pair().expect("network wire");
        let net = netgate::run(
            net_wire.into(),
            netgate::Config {
                policy: Arc::new(ral_core::egress::NetPolicy::for_test(&[])),
                audit: netgate::AuditLog::open(
                    run_dir.make_dir("network").expect("audit directory"),
                )
                .expect("audit ledger"),
                dialer: Arc::new(netgate::System),
            },
        )
        .expect("network session");
        let guest = Guest { factory, host, net };
        let root = guest.handle();
        assert_eq!(root.id(), guest.host.roots()[0].id());
        assert!(std::ptr::eq(guest.feed(), guest.host.feed()));
        assert_eq!(guest.run_dir().path(), run_dir.path());

        root.steer("go".into()).expect("live root accepts input");
        assert_eq!(root.queued(), ["go"]);
        assert!(root.release());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut opened = None;
        'job: loop {
            let signal = guest
                .feed()
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the host publishes a settled job");
            if let Signal::Commit(id, commit) = signal
                && id == root.id()
            {
                for recorded in commit.records() {
                    match recorded.value() {
                        Record::Forensic(Forensic::JobOpened) => {
                            assert!(opened.replace(JobId::from(commit.number())).is_none());
                        }
                        Record::Forensic(Forensic::JobSettled { job, stopped }) => {
                            assert_eq!(Some(*job), opened);
                            assert!(!stopped);
                            break 'job;
                        }
                        _ => {}
                    }
                }
            }
        }

        let cause = ral_core::carrier::Severed::Faulted("fixture engine loss".into());
        let lost = EngineLost::running(&cause, guest.run_dir());
        guest.capture(&lost);
        let diagnostic = std::fs::read_to_string(run_dir.path().join(FILE)).expect("captured loss");
        assert!(diagnostic.contains("fixture engine loss"));
        assert!(!shutdown.load(Ordering::SeqCst));

        guest.end().expect("joined host permits machine recovery");
        assert!(!root.is_live());
        assert!(weak_factory.upgrade().is_none());
        assert!(shutdown.load(Ordering::SeqCst));
        drop(net_peer);
    }

    #[test]
    fn one_engine_is_dialled_and_the_host_end_returned() {
        let guest_ends = Arc::new(Mutex::new(Vec::new()));
        let factory = MachineFactory(Mutex::new(Box::new(FakeMachine {
            guest_ends: Arc::clone(&guest_ends),
            shutdown: Arc::default(),
        })));

        let mut host = factory.engine().expect("the fake machine dials");
        let mut ends = guest_ends.lock_ignore_poison();
        assert_eq!(ends.len(), 1, "exactly one engine dial was made");
        let mut guest = ends.pop().expect("one dial was made");
        drop(ends);

        guest.write_all(b"ack").expect("the guest writes");
        let mut buf = [0u8; 3];
        host.read_exact(&mut buf).expect("the host reads it");
        assert_eq!(&buf, b"ack");

        drop(factory.0.into_inner_ignore_poison());
    }
}
