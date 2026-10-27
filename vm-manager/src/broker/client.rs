//! Only the folder and read-only flag cross to the privileged service.
//! The pipe connection leases the machine; sockets are duplicated for this process.

use std::io;
use std::os::windows::io::{FromRawSocket, OwnedSocket};

use super::wire::{PIPE, Reply, Request, VERSION, frame};
use crate::error::{Error, GuestConsole, Wire};
use crate::hcs::win::HYPERVISOR;
use crate::machine::{Booted, Hypervisor, Machine, Socket, Wires};
use crate::site::{Silent, site};
use crate::spec::MachineSpec;

pub struct Brokered;

impl Brokered {
    /// An unavailable broker lets detection try the direct backend.
    pub fn available() -> bool {
        connect().is_ok()
    }
}

impl Hypervisor for Brokered {
    /// # Errors
    /// Returns the folder, connection, protocol or service refusal unchanged.
    fn boot(&self, spec: &MachineSpec) -> Result<Booted, Error> {
        let folder = spec.resolve()?;
        let mut pipe = connect().map_err(|cause| {
            Error::backend(
                HYPERVISOR,
                format!(
                    "synod's machine service is not answering: {cause}. It is installed with synod \
                 and started by Windows; if it has been stopped, ask whoever administers this \
                 computer to start the 'Synod machine broker' service"
                ),
            )
        })?;

        frame::write(
            &mut pipe,
            &Request::Boot {
                version: VERSION,
                folder,
                read_only: spec.read_only,
            },
        )
        .map_err(|cause| {
            Error::backend(
                HYPERVISOR,
                format!("synod could not ask for a machine: {cause}"),
            )
        })?;

        match frame::read(&mut pipe) {
            Ok(Some(Reply::Booted { control, net })) => {
                let adopt = |wire: Wire| {
                    let description = match wire {
                        Wire::Control => &control,
                        Wire::Net => &net,
                    };
                    adopt_socket(description).map_err(|cause| {
                        Error::backend(
                            HYPERVISOR,
                            format!(
                                "the machine started, but {} could not be taken over: {cause}",
                                wire.noun()
                            ),
                        )
                    })
                };
                let wires = Wires {
                    control: adopt(Wire::Control)?,
                    net: adopt(Wire::Net)?,
                };
                // Only after both sockets are adopted may the broker close its copies.
                frame::write(&mut pipe, &Request::Adopted).map_err(|cause| {
                    Error::backend(
                        HYPERVISOR,
                        format!(
                            "synod took over the machine's wires but could not say so: {cause}"
                        ),
                    )
                })?;
                Ok(Booted {
                    machine: Box::new(BrokeredGuest { pipe }),
                    wires,
                })
            }
            Ok(Some(Reply::Refused(error))) => Err(error),
            Ok(Some(other)) => Err(Error::backend(
                HYPERVISOR,
                format!("synod's machine service answered a boot with {other:?}"),
            )),
            Ok(None) => Err(Error::backend(
                HYPERVISOR,
                "synod's machine service closed the connection without answering",
            )),
            Err(cause) => Err(Error::backend(
                HYPERVISOR,
                format!("synod could not read the machine service's answer: {cause}"),
            )),
        }
    }
}

pub(crate) struct BrokeredGuest {
    /// Closing the pipe, including on process death, tears the machine down.
    pipe: std::fs::File,
}

impl Machine for BrokeredGuest {
    fn dial_engine(&self) -> io::Result<Socket> {
        self.connect_guest(guest_abi::ports::FACTORY)
    }

    fn connect_guest(&self, _port: u32) -> io::Result<Socket> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the machine broker cannot yet dial a port inside its guest; it currently only \
             transfers connections opened by the guest",
        ))
    }

    /// An unreachable service yields an empty retained console while the caller reports the original failure.
    fn console(&self) -> GuestConsole {
        let exchange = || -> io::Result<Option<Reply>> {
            let mut pipe = &self.pipe;
            frame::write(&mut pipe, &Request::Console)?;
            frame::read(&mut pipe)
        };
        match exchange() {
            Ok(Some(Reply::Console(console))) => console,
            Ok(Some(_) | None) | Err(_) => GuestConsole::Kept {
                log: None,
                tail: Vec::new(),
            },
        }
    }

    /// # Errors
    /// Returns the service refusal or a backend error for an unexpected reply.
    fn shutdown(self: Box<Self>) -> Result<(), Error> {
        let mut pipe = self.pipe;
        if frame::write(&mut pipe, &Request::Stop).is_err() {
            // The service has gone, which means the machine has too: it holds
            // the only handle to it, and losing that handle stops it.
            return Ok(());
        }
        match frame::read(&mut pipe) {
            Ok(Some(Reply::Stopped(Ok(()))) | None) | Err(_) => Ok(()),
            Ok(Some(Reply::Stopped(Err(error)))) => Err(error),
            Ok(Some(other)) => Err(Error::backend(
                HYPERVISOR,
                format!("synod's machine service answered a stop with {other:?}"),
            )),
        }
    }
}

/// A busy pipe is retried: the service creates instances one at a time.
fn connect() -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match site!(Silent::BrokerPipeConnect, &options => open(PIPE)) {
            Ok(pipe) => return Ok(pipe),
            Err(cause) if std::time::Instant::now() >= deadline => return Err(cause),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}

/// # Errors
/// Returns the Winsock error or a refusal naming the mismatched description sizes.
pub fn adopt_socket(description: &[u8]) -> io::Result<OwnedSocket> {
    use windows_sys::Win32::Networking::WinSock::{
        INVALID_SOCKET, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSAPROTOCOL_INFOW,
        WSASocketW,
    };

    if description.len() != size_of::<WSAPROTOCOL_INFOW>() {
        return Err(io::Error::other(format!(
            "the machine service described a socket in {} bytes, not the {} a socket \
             description is",
            description.len(),
            size_of::<WSAPROTOCOL_INFOW>()
        )));
    }
    // SAFETY: WSAPROTOCOL_INFOW has no invalid bit patterns; zeroing creates an aligned local value.
    let mut info = unsafe { std::mem::zeroed::<WSAPROTOCOL_INFOW>() };
    // SAFETY: both regions are `size_of::<WSAPROTOCOL_INFOW>()` bytes, checked
    // above, and neither overlaps the other.
    unsafe {
        std::ptr::copy_nonoverlapping(
            description.as_ptr(),
            (&raw mut info).cast::<u8>(),
            size_of::<WSAPROTOCOL_INFOW>(),
        );
    }

    // SAFETY: info describes a socket duplicated for this process; flags preserve overlap and prevent inheritance.
    let socket = unsafe {
        WSASocketW(
            info.iAddressFamily,
            info.iSocketType,
            info.iProtocol,
            &raw const info,
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if socket == INVALID_SOCKET {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh socket this function owns and does not close on any other
    // path, so ownership transfers cleanly.
    Ok(unsafe { OwnedSocket::from_raw_socket(socket as u64) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_socket_description_is_refused() {
        let error = adopt_socket(&[0u8; 8]).expect_err("8 bytes is not a socket description");
        assert!(error.to_string().contains("not the"), "{error}");
    }
}
