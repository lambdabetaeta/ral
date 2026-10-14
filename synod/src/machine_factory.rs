//! Synod's [`exarch::agent::Factory`]: the host reaching *into* the guest over
//! [`vm_manager::Machine::connect_guest`], one engine per dial, made by the
//! guest daemon listening on [`ral_daemon::boot::FACTORY_PORT`].
//!
//! The factory owns the machine for as long as the conversation runs:
//! [`Conversation::begin`] moves it in here, the desk asks it for engines, and
//! [`Conversation::end`] takes it back with [`MachineFactory::into_machine`]
//! once the agent that held the other reference is gone.
//!
//! [`Conversation::begin`]: crate::session::Conversation::begin
//! [`Conversation::end`]: crate::session::Conversation::end

use std::sync::Mutex;

use exarch::agent::Factory;
use ral_core::protocol::channel::WireStream;
use ral_core::sync::LockExt;
use vm_manager::Machine;

/// `exarch::agent::Factory` over a real machine.
///
/// The `Mutex` is not for the dial (`connect_guest` takes `&self`) but for
/// `Sync`: [`Machine`] is declared `Send` and no more, and one
/// `Arc<dyn Factory>` shared by the desk's threads needs `Sync`.
pub struct MachineFactory {
    machine: Mutex<Box<dyn Machine>>,
}

impl MachineFactory {
    /// Take `machine` into the factory's keeping.
    #[must_use]
    pub fn new(machine: Box<dyn Machine>) -> Self {
        Self {
            machine: Mutex::new(machine),
        }
    }

    /// What the guest has said on its console — the host's one diagnostic
    /// reach into a guest whose engine has stopped talking to it, and the
    /// whole of what [`crate::session`]'s `engine_log` has to write down.
    ///
    /// It lives here rather than beside the machine because by the time a
    /// conversation has failed, the factory is the only thing still holding
    /// the machine: `seat_machine` moves it in before the agent is built, so
    /// a start that fails has no other handle to ask through.
    #[must_use]
    pub fn console(&self) -> vm_manager::GuestConsole {
        self.machine.lock_ignore_poison().console()
    }

    /// Hand the machine back, ending the factory.
    #[must_use]
    pub fn into_machine(self) -> Box<dyn Machine> {
        self.machine.into_inner_ignore_poison()
    }
}

impl Factory for MachineFactory {
    fn engine(&self) -> Result<WireStream, String> {
        let port = ral_daemon::boot::FACTORY_PORT;
        self.machine
            .lock_ignore_poison()
            .connect_guest(port)
            .map(WireStream::from)
            .map_err(|e| format!("could not dial the guest's engine factory on port {port}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Arc;

    // `GuestEnd` is the end of a local pair the test plays the guest on — the
    // twin of `vm_manager::AgentDial`, which the fake machine hands back as
    // the host's end.  A socketpair on Unix; on Windows, where there is none,
    // the loopback connection `ral_core::wire` uses for the same job.
    #[cfg(windows)]
    use std::net::TcpStream as GuestEnd;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream as GuestEnd;

    /// One connection, both ends owned here: the guest's for the test to
    /// speak through, the host's in the shape `connect_guest` returns.
    #[cfg(unix)]
    fn local_pair() -> std::io::Result<(GuestEnd, vm_manager::AgentDial)> {
        let (guest, host) = ral_core::process::cloexec_socketpair()?;
        Ok((guest, std::os::fd::OwnedFd::from(host)))
    }

    /// See the Unix twin above.  Bind-connect-accept on an ephemeral
    /// loopback port, because Windows has no `socketpair(2)`.
    #[cfg(windows)]
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:wire-pair-windows] the same in-process loopback pair `ral_core::protocol::channel::WireChannel::pair` stands on, here so a test can hold both ends: the port is ephemeral and the peer is this process, so no outside name is reached."
    )]
    fn local_pair() -> std::io::Result<(GuestEnd, vm_manager::AgentDial)> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let host = std::net::TcpStream::connect(listener.local_addr()?)?;
        let (guest, _peer) = listener.accept()?;
        Ok((guest, std::os::windows::io::OwnedSocket::from(host)))
    }

    /// A fake `Machine` whose `connect_guest` hands back one end of a local
    /// pair, parking the guest's end where the test can play the guest — no
    /// real vsock needed to see that the seam carries bytes.
    struct FakeMachine {
        guest_ends: Arc<Mutex<Vec<(u32, GuestEnd)>>>,
    }

    impl vm_manager::Machine for FakeMachine {
        fn workspace_path(&self) -> &std::path::Path {
            unimplemented!("not exercised by these tests")
        }
        fn take_wires(&mut self) -> vm_manager::Wires {
            unimplemented!("not exercised by these tests")
        }
        fn connect_guest(&self, port: u32) -> std::io::Result<vm_manager::AgentDial> {
            let (guest, host) = local_pair()?;
            self.guest_ends.lock_ignore_poison().push((port, guest));
            Ok(host)
        }
        fn shutdown(self: Box<Self>) -> Result<(), vm_manager::Error> {
            Ok(())
        }
    }

    /// The factory dials the daemon's `FACTORY_PORT`, and the stream it hands
    /// back is the host end of that connection.
    #[test]
    fn an_engine_is_dialled_on_the_factory_port_and_the_host_end_returned() {
        let guest_ends = Arc::new(Mutex::new(Vec::new()));
        let factory = MachineFactory::new(Box::new(FakeMachine {
            guest_ends: Arc::clone(&guest_ends),
        }));

        let mut host = factory.engine().expect("the fake machine dials");
        let (port, mut guest) = guest_ends
            .lock_ignore_poison()
            .pop()
            .expect("one dial was made");
        assert_eq!(port, ral_daemon::boot::FACTORY_PORT);

        guest.write_all(b"ack").expect("the guest writes");
        let mut buf = [0u8; 3];
        host.read_exact(&mut buf).expect("the host reads it");
        assert_eq!(&buf, b"ack");

        drop(factory.into_machine());
    }
}
