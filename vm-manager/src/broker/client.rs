//! Synod's side of the broker: a [`Hypervisor`] that asks rather than acts.
//!
//! From the application's point of view this is simply another backend — it
//! boots a [`MachineSpec`] and hands back a [`Machine`] whose control plane is a
//! socket, exactly as [`super::super::hcs`] does. What differs is that no
//! privileged call happens in this process at all: the machine is made by the
//! service, and what crosses back is one duplicated socket.
//!
//! That symmetry is the point. [`crate::detect`] can prefer this backend when
//! the service is installed and fall back to the direct one when it is not,
//! without anything upstream — `synod::session`, the seat, the frame protocol —
//! knowing which of the two it got.

use std::io;
use std::os::windows::io::{FromRawSocket, OwnedSocket};
use std::path::{Path, PathBuf};

use super::{PIPE, Reply, Request, VERSION, frame};
use crate::{Error, Hypervisor, Machine, MachineSpec};

/// The name shown to a user, and to [`Hypervisor::name`].
///
/// It says Hyper-V, not "broker", because the *hypervisor* is Hyper-V either
/// way: who asked it is synod's business and not the user's.
const HYPERVISOR: &str = "Hyper-V";

/// The broker-backed backend.
pub struct Brokered;

impl Brokered {
    /// Whether the service is installed and answering.
    ///
    /// Asked by [`crate::detect`] before it prefers this backend. A `false`
    /// here is not a refusal — it means the direct backend should be tried,
    /// which is what a developer running from a checkout wants.
    pub fn available() -> bool {
        connect().is_ok()
    }
}

impl Hypervisor for Brokered {
    fn name(&self) -> &'static str {
        HYPERVISOR
    }

    /// Ask the service for a machine over this spec's folder.
    ///
    /// The spec is *not* forwarded wholesale, deliberately: only the folder and
    /// the read-only flag cross, because those are the only two things the
    /// broker is willing to be told (see the module docs on what a narrow
    /// surface buys). The resolution below is still done here as well, so a
    /// missing folder is refused in the same words on both backends rather than
    /// arriving as a service's second-hand complaint.
    ///
    /// # Errors
    /// Returns [`Error`] if the spec is not usable, if the service cannot be
    /// reached or speaks another version, or if it refused — carrying its own
    /// sentence, which is already written for the person who granted the folder.
    fn boot(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>, Error> {
        let folder = spec.resolve()?;
        let mut pipe = connect().map_err(|cause| {
            unavailable(&format!(
                "synod's machine service is not answering: {cause}. It is installed with synod \
                 and started by Windows; if it has been stopped, ask your IT department to start \
                 the 'Synod machine broker' service"
            ))
        })?;

        frame::write(
            &mut pipe,
            &Request::Boot {
                version: VERSION,
                folder,
                read_only: spec.workspace.read_only,
            },
        )
        .map_err(|cause| unavailable(&format!("synod could not ask for a machine: {cause}")))?;

        match frame::read(&mut pipe) {
            Ok(Some(Reply::Booted {
                workspace,
                control,
                net,
            })) => {
                let control = adopt_socket(&control).map_err(|cause| {
                    unavailable(&format!(
                        "the machine started, but its control plane could not be taken over: \
                         {cause}"
                    ))
                })?;
                let net = adopt_socket(&net).map_err(|cause| {
                    unavailable(&format!(
                        "the machine started, but its net wire could not be taken over: {cause}"
                    ))
                })?;
                // Only now may the broker let go of its own handles on those
                // sockets — see [`Request::Adopted`] for why both the earlier
                // and the later moment are wrong.
                frame::write(&mut pipe, &Request::Adopted).map_err(|cause| {
                    unavailable(&format!(
                        "synod took over the machine's wires but could not say so: {cause}"
                    ))
                })?;
                Ok(Box::new(BrokeredGuest {
                    pipe: Some(pipe),
                    workspace,
                    wires: Some(crate::Wires { control, net }),
                }))
            }
            Ok(Some(Reply::Refused(why))) => Err(unavailable(&why)),
            Ok(Some(other)) => Err(unavailable(&format!(
                "synod's machine service answered a boot with {other:?}"
            ))),
            Ok(None) => Err(unavailable(
                "synod's machine service closed the connection without answering",
            )),
            Err(cause) => Err(unavailable(&format!(
                "synod could not read the machine service's answer: {cause}"
            ))),
        }
    }
}

/// A machine the service owns and this process drives.
pub struct BrokeredGuest {
    /// The lease. While this is open the machine lives; closing it — on
    /// [`Machine::shutdown`], on drop, or because this process died — is what
    /// tells the service to tear the machine down.
    pipe: Option<std::fs::File>,
    workspace: PathBuf,
    wires: Option<crate::Wires>,
}

impl Machine for BrokeredGuest {
    fn workspace_path(&self) -> &Path {
        &self.workspace
    }

    /// # Panics
    /// Panics if asked a second time: a machine has exactly one of each wire.
    fn take_wires(&mut self) -> crate::Wires {
        self.wires
            .take()
            .expect("a machine's two wires are taken at most once")
    }

    /// Not yet: the broker's agent-wire path arrives alongside HCS's own,
    /// once a Windows guest completes a boot at all.
    ///
    /// # Errors
    /// Always returns [`std::io::ErrorKind::Unsupported`].
    fn accept_agent(&mut self, _patience: std::time::Duration) -> io::Result<crate::AgentDial> {
        Err(crate::agent_not_supported_yet())
    }

    /// Ask the service to stop the machine, and wait for its answer.
    ///
    /// Both wires are closed first, for the same reason the direct backend
    /// closes them first: the guest's engine sees EOF on the control plane,
    /// and `ral-daemon` powers the machine off from inside. The service's own
    /// grace window then observes a machine already stopping.
    ///
    /// # Errors
    /// Returns [`Error::Unavailable`] if the service reports that the machine
    /// did not stop cleanly, or if it could not be asked at all.
    fn shutdown(mut self: Box<Self>) -> Result<(), Error> {
        self.wires = None;
        let Some(mut pipe) = self.pipe.take() else {
            return Ok(());
        };
        if frame::write(&mut pipe, &Request::Stop).is_err() {
            // The service has gone, which means the machine has too: it holds
            // the only handle to it, and losing that handle stops it.
            return Ok(());
        }
        match frame::read(&mut pipe) {
            Ok(Some(Reply::Stopped(Ok(()))) | None) | Err(_) => Ok(()),
            Ok(Some(Reply::Stopped(Err(why)))) => Err(unavailable(&why)),
            Ok(Some(other)) => Err(unavailable(&format!(
                "synod's machine service answered a stop with {other:?}"
            ))),
        }
    }
}

impl Drop for BrokeredGuest {
    /// Dropping the lease stops the machine, so a caller that forgets — or
    /// unwinds — does not leave a guest running. Nothing has to be sent: the
    /// service is reading this pipe, and its end of it going quiet is the
    /// signal.
    fn drop(&mut self) {
        self.wires = None;
        self.pipe = None;
    }
}

/// Open the pipe.
///
/// A busy pipe is the ordinary case rather than a failure: the broker creates
/// one instance at a time, so a client arriving between a connection and the
/// next instance finds none free.  Retrying is what the platform expects of a
/// client, and there is nothing else to do about it.
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: opening the broker's pipe — the client half of an IPC door, not a \
              file the model named. Host-side, before any machine exists; no shell, no run, no \
              card."
)]
fn connect() -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match OpenOptions::new().read(true).write(true).open(PIPE) {
            Ok(pipe) => return Ok(pipe),
            Err(cause) if std::time::Instant::now() >= deadline => return Err(cause),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}

/// Turn a `WSAPROTOCOL_INFOW` the service produced for *this* process back into
/// a socket.
///
/// The bytes are opaque and process-specific by design: they name a socket the
/// service already owns, describable only for the one process id it named. In
/// any other process — or after this one has used them — they are inert.
///
/// # Errors
/// Returns the Winsock error, or a sentence if the description is not the size
/// a `WSAPROTOCOL_INFOW` is (a mismatched service and client, which the version
/// check should already have caught).
pub fn adopt_socket(description: &[u8]) -> io::Result<OwnedSocket> {
    use windows_sys::Win32::Networking::WinSock::{
        INVALID_SOCKET, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSAPROTOCOL_INFOW,
        WSASocketW,
    };

    if description.len() != size_of::<WSAPROTOCOL_INFOW>() {
        return Err(io::Error::other(format!(
            "the machine service described its control plane in {} bytes, not the {} a socket \
             description is",
            description.len(),
            size_of::<WSAPROTOCOL_INFOW>()
        )));
    }
    // SAFETY: `WSAPROTOCOL_INFOW` is a plain C structure of `Copy` fields with
    // no padding invariants and no pointers, so any byte sequence of its own
    // length is a valid value of it; the bytes here came from
    // `WSADuplicateSocketW` writing exactly one of them.  It is read into a
    // local rather than borrowed from the slice so that it is aligned.
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

    // SAFETY: `info` describes a socket duplicated for this process; passing it
    // to `WSASocketW` with the family/type/protocol it itself carries is the
    // documented way to adopt one.  The flags keep the socket overlapped (as
    // the original was) and out of any child's inheritance.
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

/// An [`Error::Unavailable`] for this backend carrying `why`.
fn unavailable(why: &str) -> Error {
    Error::Unavailable {
        hypervisor: HYPERVISOR,
        why: why.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A description of the wrong length is refused with the two sizes named,
    /// rather than being reinterpreted as whatever happens to be in memory.
    #[test]
    fn a_malformed_socket_description_is_refused() {
        let error = adopt_socket(&[0u8; 8]).expect_err("8 bytes is not a socket description");
        assert!(error.to_string().contains("not the"), "{error}");
    }

    /// The backend names the hypervisor, not itself: a user is told what their
    /// machine runs on, and synod's own division of labour is not their
    /// business.
    #[test]
    fn the_backend_names_the_hypervisor() {
        assert_eq!(Brokered.name(), "Hyper-V");
    }
}
