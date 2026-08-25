//! Synod's [`exarch::agent::Dial`]: the host reaching *into* the guest over
//! [`vm_manager::Machine::connect_guest`], one dial per spawn.
//!
//! The dialler owns the machine for as long as the conversation runs:
//! [`Conversation::begin`] moves it in here, the desk dials through it, and
//! [`Conversation::end`] takes it back with [`MachineDial::into_machine`]
//! once the agent that held the other reference is gone.
//!
//! [`Conversation::begin`]: crate::session::Conversation::begin
//! [`Conversation::end`]: crate::session::Conversation::end

use std::sync::Mutex;

use exarch::agent::Dial;
use ral_core::wire::WireStream;
use vm_manager::Machine;

/// `exarch::agent::Dial` over a real machine.
///
/// The `Mutex` is not for the dial — `connect_guest` takes `&self` — but for
/// `Sync`: [`Machine`] is declared `Send` and no more, and one
/// `Arc<dyn Dial>` shared by the desk's threads needs `Sync`.
pub struct MachineDial {
    machine: Mutex<Box<dyn Machine>>,
}

impl MachineDial {
    /// Take `machine` into the dialler's keeping.
    #[must_use]
    pub fn new(machine: Box<dyn Machine>) -> Self {
        Self {
            machine: Mutex::new(machine),
        }
    }

    /// Hand the machine back, ending the dialler.
    #[must_use]
    pub fn into_machine(self) -> Box<dyn Machine> {
        self.machine
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Dial for MachineDial {
    fn dial(&self, port: u32) -> Result<WireStream, String> {
        self.machine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connect_guest(port)
            .map(WireStream::from)
            .map_err(|e| format!("could not dial the guest's listener on port {port}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    /// A fake `Machine` whose `connect_guest` hands back one end of a
    /// socketpair, parking the guest's end where the test can play the
    /// guest — no real vsock needed to see that the seam carries bytes.
    struct FakeMachine {
        guest_ends: Arc<Mutex<Vec<(u32, UnixStream)>>>,
    }

    impl vm_manager::Machine for FakeMachine {
        fn workspace_path(&self) -> &std::path::Path {
            unimplemented!("not exercised by these tests")
        }
        fn take_wires(&mut self) -> vm_manager::Wires {
            unimplemented!("not exercised by these tests")
        }
        fn connect_guest(&self, port: u32) -> std::io::Result<vm_manager::AgentDial> {
            let (guest, host) = UnixStream::pair()?;
            self.guest_ends
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((port, guest));
            Ok(OwnedFd::from(host))
        }
        fn shutdown(self: Box<Self>) -> Result<(), vm_manager::Error> {
            Ok(())
        }
    }

    /// The dial reaches the machine with the port it was given, and the
    /// stream it hands back is the host end of that connection.
    #[test]
    fn a_dial_carries_the_port_and_hands_back_the_host_end() {
        let guest_ends = Arc::new(Mutex::new(Vec::new()));
        let dialler = MachineDial::new(Box::new(FakeMachine {
            guest_ends: Arc::clone(&guest_ends),
        }));

        let mut host = dialler.dial(1732).expect("the fake machine dials");
        let (port, mut guest) = guest_ends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .expect("one dial was made");
        assert_eq!(port, 1732);

        guest.write_all(b"ack").expect("the guest writes");
        let mut buf = [0u8; 3];
        host.read_exact(&mut buf).expect("the host reads it");
        assert_eq!(&buf, b"ack");

        drop(dialler.into_machine());
    }
}
