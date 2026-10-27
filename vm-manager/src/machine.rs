use std::io;

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
    /// Returns the error from connecting to the guest's factory port.
    fn dial_engine(&self) -> io::Result<Socket>;

    /// # Errors
    /// Returns [`io::ErrorKind::Unsupported`] or the backend's dial error.
    fn connect_guest(&self, port: u32) -> io::Result<Socket>;

    fn console(&self) -> GuestConsole;

    /// # Errors
    /// Returns [`Error`] if the machine did not stop cleanly.
    fn shutdown(self: Box<Self>) -> Result<(), Error>;
}
