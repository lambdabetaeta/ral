use crate::dial::DialError;
use crate::error::{Error, GuestConsole};
use crate::spec::MachineSpec;

pub trait Hypervisor {
    /// # Errors
    /// Returns the spec or backend's boot refusal.
    fn boot(&self, spec: &MachineSpec) -> Result<Booted, Error>;
}

#[cfg(unix)]
pub type Socket = std::os::fd::OwnedFd;

#[cfg(windows)]
pub type Socket = std::os::windows::io::OwnedSocket;

/// The host ends of the control-plane and network wires, transferred together.
#[derive(Debug)]
pub struct Wires {
    pub control: Socket,
    pub net: Socket,
}

pub struct Booted {
    pub machine: Box<dyn Machine>,
    pub wires: Wires,
}

pub trait Machine: Send {
    /// # Errors
    /// Returns the typed failure from connecting to the guest's factory port.
    fn dial_engine(&self) -> Result<Socket, DialError>;

    /// # Errors
    /// Returns [`DialError::Unsupported`] or the backend's typed dial failure.
    fn connect_guest(&self, port: u32) -> Result<Socket, DialError>;

    fn console(&self) -> GuestConsole;

    /// # Errors
    /// Returns [`Error`] if the machine did not stop cleanly.
    fn shutdown(self: Box<Self>) -> Result<(), Error>;
}
