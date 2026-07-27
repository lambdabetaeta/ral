//! The machine broker: how synod starts a virtual machine without being
//! privileged.
//!
//! # The problem
//!
//! [`super::hcs`] can create a machine only for a caller the compute service
//! serves — an administrator, or a member of the local *Hyper-V
//! Administrators* group. Synod's user is a university secretary, and neither
//! is acceptable for her. Being an administrator is obviously wrong. Being in
//! that group is *quietly* wrong, and worth spelling out because it looks like
//! the cheap answer: it permits attaching a **physical disk** to a virtual
//! machine, and a guest reading a raw disk reads past every NTFS permission on
//! it. Putting every synod user in that group would hand each of them a local
//! privilege escalation in order to run an application whose entire claim is
//! that it can see one folder and nothing else. The remedy would defeat the
//! thing it was meant to enable.
//!
//! # The shape of the answer, and whose it is
//!
//! So the privilege moves out of the application and into a small service the
//! installer registers: `LocalSystem` runs the machine's lifecycle, the user
//! runs synod. This is not an invention — it is how every product in this
//! position on Windows is built. WSL has `wslservice`, Docker has
//! `com.docker.service`, `VirtualBox` has `VBoxSDS`; in each case an
//! unprivileged client asks a privileged broker to make a machine, and the
//! broker hands back what the client needs to use it.
//!
//! What makes a broker safe is not its size but the *narrowness of what it
//! accepts*. This one takes exactly one instruction — boot a machine over this
//! folder — and constructs the machine itself, from its own installed boot
//! media, in its own cache directory, with its own fixed device list. None of
//! the parameters that would make a broker dangerous is client-controlled:
//!
//! - the **kernel, initramfs and rootfs** are the ones installed beside the
//!   broker ([`service::media`]), never paths a caller names;
//! - the **devices** are the ones `vm-manager/src/hcs/spec.rs` writes: two disks,
//!   one share, one socket, one console, and no network adapter. There is no
//!   pass-through of anything, so there is no request that could ask for one;
//! - the **folder** is checked against the *caller's own* rights, not the
//!   service's, by impersonating the client across the pipe
//!   ([`service::readable_by_client`]). Without that check a broker running as
//!   `LocalSystem` would happily mount one user's documents into another
//!   user's guest, which is a far worse hole than the one it was built to
//!   close;
//! - the **resources** are clamped to what a session may ask for, so a caller
//!   cannot request a machine larger than the computer.
//!
//! # How a socket crosses a process boundary
//!
//! The broker accepts the guest's control-plane connection, but the *engine's*
//! frames have to reach synod. Windows' answer is
//! `WSADuplicateSocketW`: the owner of a socket asks for a description of it
//! valid for one named process, and that process turns the description back
//! into a socket of its own. Both ends are then peers on the same connection,
//! and the broker closes its own handle.
//!
//! The process it is duplicated *for* comes from `GetNamedPipeClientProcessId`
//! — the kernel's own answer to "who is on the other end of this pipe" — never
//! from anything the client said. A client that lied about its process id could
//! otherwise have a live socket into a guest planted in someone else's process.
//!
//! # The connection is the lease
//!
//! A machine lives exactly as long as the pipe connection that asked for it.
//! The broker holds the [`Machine`](crate::Machine) in the thread serving that
//! client; when the client asks it to stop, or exits, or crashes, the thread
//! drops it and the machine is torn down with its session disk. Nothing has to
//! be reaped later, and no bookkeeping outlives the process that owns it —
//! which is the same law `dev/docs/VM/SYNOD.md` §3 gives the engine's own wire,
//! applied one layer down.

use serde::{Deserialize, Serialize};

pub mod client;
pub mod service;

/// The pipe both halves meet on.
///
/// One instance per client, all under the one name; the pipe's own security
/// descriptor ([`service::PIPE_SDDL`]) is what decides who may connect at all.
pub const PIPE: &str = r"\\.\pipe\synod-machine-broker";

/// The protocol version both ends check before anything else.
///
/// An installed service and a freshly built synod can differ in age — the MSI
/// installs both, but a developer runs one from `target\debug` against the
/// other from `Program Files` — so a mismatch has to be refused loudly rather
/// than discovered as a strange field later. The same law
/// `dev/docs/VM/SYNOD.md` §3 puts on the engine's own `Attach`.
///
/// 2, since [`Reply::Booted`] grew a second socket description: an installed
/// service still speaking 1 would answer a boot with one wire, and a client
/// that adopted only that one would have no net wire to hand the engine.
pub const VERSION: u32 = 2;

/// What synod asks the broker for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Boot a machine over `folder`. The only instruction the broker takes.
    ///
    /// Everything about the machine other than these three fields is the
    /// broker's own, and the three are checked rather than trusted: the folder
    /// against the caller's rights, the resources against the computer's.
    Boot {
        /// The protocol version the client speaks.
        version: u32,
        /// The folder the user granted, as the *client's* path.
        folder: std::path::PathBuf,
        /// Whether the agent may only read it.
        read_only: bool,
    },
    /// The client has made its own sockets from the descriptions it was
    /// given, so the broker may close the two it still holds.
    ///
    /// This step exists because of an ordering trap with two jaws, and it now
    /// closes on two handles rather than one. Closing the broker's own
    /// before the client has adopted its descriptions would race the
    /// duplication and could leave the client with a socket whose connection
    /// is already gone. Never closing them would be worse and quieter: two
    /// handles apiece hold each connection open, so the guest would not see
    /// the end-of-file that a closing client is supposed to cause on either
    /// wire, and the inside-out power-off every teardown depends on would
    /// simply never start — a machine that shuts down only when forced, for
    /// a reason no log would show. So the client says when it is safe, and
    /// only then does the broker let go — of both, in one statement, for the
    /// same reason [`crate::hcs::Guest`]'s own wires are dropped together.
    Adopted,
    /// Stop the machine this connection owns, and report whether it stopped
    /// cleanly. Dropping the connection stops it too; this exists so a caller
    /// that wants to *report* a failed shutdown can have one.
    Stop,
}

/// What the broker answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Reply {
    /// The machine is up and its guest has dialled both wires.
    Booted {
        /// Where the granted folder appears inside the guest — the path the
        /// agent works at, never a host path.
        workspace: std::path::PathBuf,
        /// The control-plane socket, described for this client's process by
        /// `WSADuplicateSocketW`; [`client::adopt_socket`] turns it back into a
        /// socket. Opaque bytes on the wire, and meaningless in any other
        /// process.
        control: Vec<u8>,
        /// The net-wire socket, described the same way as `control` and for
        /// the same process — the two are peers, not a pair to be told apart
        /// by which arrives first.
        net: Vec<u8>,
    },
    /// The machine stopped; `Err` carries the sentence explaining what was not
    /// clean about it.
    Stopped(Result<(), String>),
    /// The request was refused, in words fit to show the person who granted the
    /// folder.
    Refused(String),
}

/// Length-prefixed JSON, the same framing the engine's own wire uses.
///
/// Written out here rather than borrowed from `ral-core`'s `subprocess_codec`
/// because this crate deliberately does not depend on `ral-core` — a machine
/// layer that needed the shell to talk to its own service would have the
/// dependency backwards.
mod frame {
    use std::io::{self, Read, Write};

    /// The largest frame either end will allocate for. A broker's messages are
    /// a path and a socket description; anything above this is a fault or an
    /// attack, and is refused before a byte is allocated.
    const MAX: u32 = 64 * 1024;

    /// # Errors
    /// Returns the write or encode error.
    pub(super) fn write<W: Write, T: serde::Serialize>(w: &mut W, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        let len = u32::try_from(bytes.len())
            .ok()
            .filter(|len| *len <= MAX)
            .ok_or_else(|| io::Error::other("broker: frame too large"))?;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(&bytes)?;
        w.flush()
    }

    /// # Errors
    /// Returns the read or decode error; `Ok(None)` on a clean end of stream —
    /// which for the service means the client has gone, and so the machine
    /// should.
    pub(super) fn read<R: Read, T: serde::de::DeserializeOwned>(
        r: &mut R,
    ) -> io::Result<Option<T>> {
        let mut prefix = [0u8; 4];
        match r.read_exact(&mut prefix) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(prefix);
        if len > MAX {
            return Err(io::Error::other(format!(
                "broker: frame of {len} bytes exceeds the {MAX}-byte limit"
            )));
        }
        let mut body = vec![0u8; len as usize];
        r.read_exact(&mut body)?;
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request round-trips through the framing, and a reply after it on the
    /// same stream — the protocol is a conversation, not one message.
    #[test]
    fn frames_round_trip_in_order() {
        let mut buffer = Vec::new();
        frame::write(
            &mut buffer,
            &Request::Boot {
                version: VERSION,
                folder: std::path::PathBuf::from(r"C:\Users\secretary\Invoices"),
                read_only: false,
            },
        )
        .unwrap();
        frame::write(&mut buffer, &Request::Stop).unwrap();

        let mut cursor = std::io::Cursor::new(buffer);
        let first: Request = frame::read(&mut cursor).unwrap().unwrap();
        assert!(matches!(first, Request::Boot { version, .. } if version == VERSION));
        assert!(matches!(
            frame::read::<_, Request>(&mut cursor).unwrap(),
            Some(Request::Stop)
        ));
        assert!(
            frame::read::<_, Request>(&mut cursor).unwrap().is_none(),
            "a spent stream reads as gone, not as an error"
        );
    }

    /// A length prefix above the limit is refused before anything is
    /// allocated for it: the one thing a hostile client on this pipe could
    /// otherwise ask a privileged service to do is exhaust its memory.
    #[test]
    fn an_oversized_frame_is_refused_before_allocation() {
        let mut stream = std::io::Cursor::new(u32::MAX.to_le_bytes().to_vec());
        let error = frame::read::<_, Request>(&mut stream).expect_err("refused");
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    /// The whole broker, over a real pipe, in one process: a served instance, a
    /// client that finds it, a request that crosses, the folder checked against
    /// the caller, and an answer that comes back.
    ///
    /// What it can prove depends on the account it runs under, and both answers
    /// are correct — the same shape `crate::detect`'s own test takes. Where the
    /// compute service serves this user, a machine really boots and the control
    /// socket really crosses the process boundary. Where it does not, the
    /// refusal has still travelled the entire path — pipe descriptor, framing,
    /// impersonation, the folder check, the reply — which is everything the
    /// broker adds over the backend it wraps.
    ///
    /// Skipped when a real service is installed: the pipe has one name, and
    /// stealing it from a running broker would be a poor way to test it.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "REASONED-SILENT: test scaffolding makes the folder it then grants; no shell, no \
                  run, no card."
    )]
    fn the_broker_answers_a_client_over_a_real_pipe() {
        use crate::{Hypervisor, MachineSpec};

        if client::Brokered::available() {
            eprintln!("a machine broker is already installed on this computer; not displacing it");
            return;
        }
        std::thread::Builder::new()
            .name("test-broker".to_string())
            .spawn(|| {
                let _ = service::serve();
            })
            .expect("a thread to serve on");

        assert!(
            client::Brokered::available(),
            "the client must find the pipe the service just opened"
        );

        let folder = std::env::temp_dir().join("synod-broker-test-folder");
        std::fs::create_dir_all(&folder).expect("a folder to grant");
        let outcome = client::Brokered.boot(&MachineSpec::for_folder(&folder));

        match outcome {
            Ok(machine) => {
                // This account may create machines, so the guest booted and its
                // control plane crossed a process boundary.
                assert_eq!(machine.workspace_path(), std::path::Path::new("/work"));
                machine.shutdown().expect("the machine stops cleanly");
            }
            Err(refused) => {
                let text = refused.to_string();
                assert!(
                    text.contains("Hyper-V Administrators")
                        || text.contains("Virtual Machine Platform")
                        || text.contains("could not find")
                        || text.contains("virtual-machine service"),
                    "the refusal must have come from the machine layer, having crossed the pipe: \
                     {text}"
                );
            }
        }
    }
}
