//! The factory owns the machine until the settled driver and agent release it.

use std::io::Write;
use std::sync::{Arc, Mutex};

use exarch::agent::{Avatar, Factory, Helm, RootConfig, RootSeat};
use exarch::bus::Sink;
use exarch::headless::SettledConversation;
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
    settled: SettledConversation,
    net: netgate::Session,
}

impl Guest {
    pub(crate) fn seat(
        booted: Booted,
        mut config: RootConfig,
        ceiling: Capabilities,
        provider: Arc<Provider>,
        net: netgate::Config,
    ) -> Result<Self, String> {
        let Booted { machine, wires } = booted;
        let root_seat = RootSeat::guest(
            wires.control.into(),
            GuestPath::from_static(guest_abi::layout::WORK),
            GuestPath::from_static(guest_abi::layout::SCRATCH),
            ceiling,
        )?;
        let factory = Arc::new(MachineFactory(Mutex::new(machine)));
        config.factory = Some(factory.clone());
        let agent = Avatar::root(config, root_seat, provider)
            .map_err(|why| start_failed(&factory, &why))?;
        let settled = SettledConversation::new(agent)?;
        let net = netgate::run(wires.net.into(), net)
            .map_err(|why| format!("could not start guest networking: {why}"))?;
        Ok(Self {
            factory,
            settled,
            net,
        })
    }

    pub(crate) fn helm(&self) -> Helm {
        self.settled.root().helm()
    }

    pub(crate) fn await_message(&self) -> bool {
        self.settled.await_message()
    }

    pub(crate) fn exchange<S: Sink>(&mut self, sink: &mut S) -> Result<(), String> {
        self.settled.exchange(sink)
    }

    pub(crate) fn severance(&self) -> Option<String> {
        let cause = self.settled.root().severance()?;
        let run_dir = self.settled.root().run_dir();
        let lost = exarch::agent::EngineLost::running(&cause, run_dir);
        capture(run_dir, &lost.logged(), &self.console());
        Some(lost.to_string())
    }

    pub(crate) fn console(&self) -> GuestConsole {
        self.factory.console()
    }

    pub(crate) fn end(self) -> Result<(), String> {
        let Self {
            factory,
            settled,
            net,
        } = self;
        // Control EOF and fleet teardown precede machine recovery, net join, and shutdown.
        settled.end_fleet();
        let machine = Arc::into_inner(factory)
            .expect("settled conversation teardown must release every guest factory owner")
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
    use std::io::{Read, Write};

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
            Ok(())
        }
    }

    #[test]
    fn one_engine_is_dialled_and_the_host_end_returned() {
        let guest_ends = Arc::new(Mutex::new(Vec::new()));
        let factory = MachineFactory(Mutex::new(Box::new(FakeMachine {
            guest_ends: Arc::clone(&guest_ends),
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
