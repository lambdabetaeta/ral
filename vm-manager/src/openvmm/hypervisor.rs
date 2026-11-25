//! Raw SCSI disks, a virtio-fs share and a hybrid vsock relay over `AF_UNIX`; no network adapter.
//! The machine lives exactly as long as the process, and the guest stops only from the inside.

#![allow(
    clippy::disallowed_macros,
    reason = "[silent:windows-host] a host diagnostic with no ral shell to print through"
)]

use std::io::{self, Read as _, Write as _};
use std::net::TcpStream;
use std::os::windows::io::OwnedSocket;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use guest_abi::{layout::WORKSPACE_NAME, ports};

use super::afunix::{self, Address, Listener};
use super::console::{self, Console, Log};
use super::process::{self, Vmm};
use super::proto::{self, Exit};
use super::win::{self, HYPERVISOR};
use super::{relay, ttrpc};
use crate::dial::DialError;
use crate::error::{Act, Error, GuestConsole, HostFile, MachineFiles, PowerEvent, Wire};
use crate::lifecycle::{
    BOOT_TIMEOUT, GUEST_DIAL_PATIENCE, Missed, PULSE, RPC_SOCKET, SESSION_BYTES, SessionDisk,
    VSOCK, aside_spelling, await_dials, stop_within, sweep, unix_seconds,
};
use crate::machine::{Booted, Hypervisor, Machine, Socket};
use crate::media;
use crate::site::{Silent, site};
use crate::spec::{BootArtifact, MachineSpec, SIZE, boot_line};

/// # Errors
/// Returns [`Error::HypervisorPlatformOff`], then [`Error::NoVmm`].
pub(crate) fn available() -> Result<PathBuf, Error> {
    if !hypervisor_present() {
        return Err(Error::HypervisorPlatformOff);
    }
    locate()
}

/// Asks WHP itself, loaded only from System32, so a planted DLL cannot answer.
fn hypervisor_present() -> bool {
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    use windows_sys::core::HRESULT;

    /// Declared from `WinHvPlatform.h`, so no hypervisor bindings are linked.
    type GetCapability = unsafe extern "system" fn(
        code: i32,
        buffer: *mut std::ffi::c_void,
        size: u32,
        written: *mut u32,
    ) -> HRESULT;
    const HYPERVISOR_PRESENT: i32 = 0;

    let name = win::wide("WinHvPlatform.dll".as_ref());
    // SAFETY: a NUL-terminated name, searched for only in System32.
    let module = unsafe {
        LoadLibraryExW(
            name.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return false;
    }
    // SAFETY: a live module and a NUL-terminated export name.
    let entry = unsafe { GetProcAddress(module, c"WHvGetCapability".as_ptr().cast()) };
    let present = entry.is_some_and(|entry| {
        // SAFETY: the export has `GetCapability`'s signature.
        let get = unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, GetCapability>(entry)
        };
        let (mut present, mut written) = (0u32, 0u32);
        // SAFETY: this code fills a four-byte BOOL; both slots are writable.
        let hr = unsafe {
            get(
                HYPERVISOR_PRESENT,
                (&raw mut present).cast(),
                4,
                &raw mut written,
            )
        };
        hr >= 0 && present != 0
    });
    // SAFETY: nothing from the module outlives this call.
    unsafe { FreeLibrary(module) };
    present
}

/// Beside the running executable, then in its checkout's `target/openvmm`; never on `PATH`.
fn locate() -> Result<PathBuf, Error> {
    let exe = std::env::current_exe().map_err(|cause| {
        Error::backend(
            HYPERVISOR,
            format!("synod could not tell where it is installed, and so where OpenVMM is: {cause}"),
        )
    })?;
    let looked_in: Vec<PathBuf> = [
        site!(Silent::CheckoutRoot, exe.as_path() => parent()).map(|dir| dir.join("openvmm.exe")),
        media::checkout(&exe).map(|root| root.join("target").join("openvmm").join("openvmm.exe")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let found = looked_in
        .iter()
        .find(|path| site!(Silent::CheckoutRoot, path => is_file()))
        .cloned();
    found.ok_or(Error::NoVmm { looked_in })
}

pub(crate) struct OpenVmm {
    artifact: BootArtifact,
    vmm: PathBuf,
    sessions: PathBuf,
    logs: PathBuf,
}

impl OpenVmm {
    /// Construction sweeps abandoned session directories before starting a new session.
    pub(crate) fn new(artifact: BootArtifact, vmm: PathBuf, sessions: PathBuf) -> Self {
        sweep(&sessions);
        let logs = site!(Silent::LocalAppDataVar, std::env::var_os("LOCALAPPDATA"))
            .map_or_else(
                || site!(Silent::LocalAppDataFallback, std::env::temp_dir()),
                PathBuf::from,
            )
            .join("Synod")
            .join("Machine");
        Self {
            artifact,
            vmm,
            sessions,
            logs,
        }
    }
}

impl Hypervisor for OpenVmm {
    /// # Errors
    /// Returns the folder, media, path, session-disk, `OpenVMM` or guest-dial refusal.
    fn boot(&self, spec: &MachineSpec) -> Result<Booted, Error> {
        let folder = win::plain(&spec.resolve()?)?;
        let artifact = self.artifact.resolve()?;
        // Every session path is a fresh plain name under this one, so only it can be misread.
        let sessions = win::plain(&self.sessions).map_err(|error| match error {
            Error::NoPlainSpelling(path) => Error::TempNoPlainSpelling(path),
            error => error,
        })?;
        let session = SessionDisk::create(&sessions, |disk| win::sparse(disk, SESSION_BYTES))?;
        let files = MachineFiles {
            kernel: win::plain(&artifact.kernel)?,
            initramfs: win::plain(&artifact.initramfs)?,
            rootfs: win::plain(&artifact.rootfs)?,
            session: win::plain(session.path())?,
            folder,
        };
        let text = win::document_text;
        let disks = [
            proto::Disk {
                lun: 0,
                path: text(&files.rootfs)?,
                read_only: true,
            },
            proto::Disk {
                lun: 1,
                path: text(&files.session)?,
                read_only: false,
            },
        ];
        let shares = [proto::Share {
            tag: WORKSPACE_NAME,
            root: text(&files.folder)?,
            read_only: spec.read_only,
        }];
        let socket = |name: &str| Address::new(&session.dir().join(name));
        let rpc = socket(RPC_SOCKET)?;
        let vsock = socket(VSOCK)?;
        let listener = |port: u32| socket(&format!("{VSOCK}_{port}"));
        let (control_at, net_at) = (listener(ports::CONTROL)?, listener(ports::NET)?);

        let id = win::fresh_id();
        // A machine without diagnostics still boots.
        let console = Console::create(&id, &self.logs)
            .inspect_err(|why| aside("the guest's console could not be opened", why))
            .ok();
        let pipe = console.as_ref().map(Console::pipe);
        let cmdline = boot_line("ttyS0", unix_seconds());
        let create = proto::create_vm(&proto::Machine {
            memory_mb: u64::from(SIZE.memory_mib),
            processors: clamp_cpus(SIZE.vcpus),
            disks: &disks,
            shares: &shares,
            console_pipe: pipe.as_deref(),
            kernel: text(&files.kernel)?,
            initrd: text(&files.initramfs)?,
            cmdline: &cmdline,
            hvsocket: VSOCK,
        });
        let log = console::open_log(Log::Vmm, &self.logs, &id)
            .inspect_err(|why| aside("OpenVMM's log will not be kept", why))
            .ok();

        // Declared after `session`, so any refusal below ends the process before its disk goes;
        // a refusal keeps both logs, which it may name.
        let deadline = Instant::now() + BOOT_TIMEOUT;
        let vmm = process::start(&self.vmm, session.dir(), log)?;
        let mut rpcs = ttrpc::Client::new(vmm.connect(&rpc, BOOT_TIMEOUT)?);
        let refused = |call: &str, failure: ttrpc::Failure| {
            let why = format!("failed its {call} call: {failure}");
            match failure {
                ttrpc::Failure::Io(_) | ttrpc::Failure::TimedOut => {
                    vmm.failed_unless_exited(&format!("during its {call} call"), why)
                }
                ttrpc::Failure::Status { .. } | ttrpc::Failure::Malformed(_) => vmm.failed(why),
            }
        };
        rpcs.call(ttrpc::Method::CreateVm, &create, deadline)
            .map_err(|failure| match failure {
                ttrpc::Failure::Status { message, .. } => Error::NotConstructed {
                    hypervisor: HYPERVISOR,
                    why: message,
                    files: Box::new(files.clone()),
                },
                failure => refused("CreateVM", failure),
            })?;
        // Both listeners exist before the guest can first dial.
        let bind = |at: &Address| {
            Listener::bind(at).map_err(Error::host_file(HostFile::Socket, Act::Bind, at.path()))
        };
        let (control, net) = (bind(&control_at)?, bind(&net_at)?);
        rpcs.call(ttrpc::Method::ResumeVm, &[], deadline)
            .map_err(|failure| refused("ResumeVM", failure))?;
        let wires = await_dials(
            HYPERVISOR,
            |wire, patience| {
                let listener = match wire {
                    Wire::Control => &control,
                    Wire::Net => &net,
                };
                accept(&vmm, listener, wire, patience)
            },
            || console.as_ref().map(kept),
            vmm.log(),
        )?;
        Ok(Booted {
            machine: Box::new(Guest {
                live: Some((vmm, session)),
                vsock,
                console,
            }),
            wires,
        })
    }
}

fn aside(what: &str, why: &Error) {
    eprintln!("synod: {what}: {}", why.worded(aside_spelling));
}

/// Accepts in short slices, so a guest that ends before dialling is reported at once.
fn accept(
    vmm: &Vmm,
    listener: &Listener,
    wire: Wire,
    patience: Duration,
) -> Result<Socket, Missed> {
    let deadline = Instant::now() + patience;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Missed::Timeout);
        }
        match listener.accept_within(left.min(PULSE)) {
            Ok(socket) => return Ok(socket),
            Err(cause) if cause.kind() == io::ErrorKind::TimedOut => {}
            Err(cause) => {
                return Err(Missed::Failed(Error::backend(
                    HYPERVISOR,
                    format!("{} could not be accepted during boot: {cause}", wire.noun()),
                )));
            }
        }
        match vmm.exited() {
            Ok(None) => {}
            Ok(Some(code)) => {
                return Err(match proto::exit(code) {
                    Exit::Guest(event) => Missed::Ended(event),
                    Exit::Vmm(code) => Missed::Failed(vmm.failed(process::status(code))),
                });
            }
            Err(cause) => {
                return Err(Missed::Failed(vmm.failed(format!(
                    "could not be watched while the guest booted: {cause}"
                ))));
            }
        }
    }
}

fn kept(console: &Console) -> GuestConsole {
    GuestConsole::Kept {
        log: console.log().map(Path::to_path_buf),
        tail: console.tail(),
    }
}

/// Exists only once both wires are dialled, so a clean stop may discard its logs.
struct Guest {
    /// Taken by the first stop; the process is gone before its session disk.
    live: Option<(Vmm, SessionDisk)>,
    vsock: Address,
    console: Option<Console>,
}

impl Guest {
    /// # Errors
    /// Returns the guest's own power event, `OpenVMM`'s failure, or `ForcedStop` past the grace.
    fn stop(&mut self) -> Result<(), Error> {
        let Some((vmm, session)) = self.live.take() else {
            return Ok(());
        };
        // Every caller has closed the wires, so the guest is already powering off.
        let outcome = stop_within(
            HYPERVISOR,
            || {},
            || match vmm.exited() {
                Ok(None) => Ok(false),
                Ok(Some(code)) => match proto::exit(code) {
                    Exit::Guest(PowerEvent::PowerOff) => Ok(true),
                    Exit::Guest(event) => Err(Error::backend(HYPERVISOR, event.clause())),
                    Exit::Vmm(code) => Err(vmm.failed(process::status(code))),
                },
                Err(cause) => Err(Error::backend(
                    HYPERVISOR,
                    format!("synod could not tell whether OpenVMM had exited: {cause}"),
                )),
            },
            || {
                vmm.kill()
                    .map_err(|cause| vmm.failed(format!("could not be stopped: {cause}")))
            },
        );
        // A refusal may name either log, so only a clean stop discards them.
        if outcome.is_ok() {
            console::discard(self.console.as_ref().and_then(Console::log));
            console::discard(vmm.log());
        }
        drop(vmm);
        drop(session);
        outcome
    }
}

impl Machine for Guest {
    fn dial_engine(&self) -> Result<Socket, DialError> {
        self.connect_guest(ports::FACTORY)
    }

    /// # Errors
    /// Returns the relay's refusal, [`DialError::Unanswered`] or [`DialError::TimedOut`].
    fn connect_guest(&self, port: u32) -> Result<Socket, DialError> {
        let deadline = Instant::now() + GUEST_DIAL_PATIENCE;
        let stream = TcpStream::from(afunix::connect_to(&self.vsock).map_err(DialError::Io)?);
        let timed_out = DialError::TimedOut {
            port,
            within: GUEST_DIAL_PATIENCE,
        };
        stream
            .set_write_timeout(Some(GUEST_DIAL_PATIENCE))
            .map_err(DialError::Io)?;
        (&stream)
            .write_all(relay::request(port).as_bytes())
            .map_err(DialError::Io)?;
        let mut reply = Vec::new();
        while reply.last() != Some(&b'\n') {
            if reply.len() == relay::REPLY_LIMIT {
                return Err(unexpected(port, &reply));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(timed_out);
            }
            stream.set_read_timeout(Some(left)).map_err(DialError::Io)?;
            // One byte at a time: what follows the newline is the guest's.
            let mut byte = 0;
            match (&stream).read(std::slice::from_mut(&mut byte)) {
                Ok(0) if reply.is_empty() => return Err(DialError::Unanswered { port }),
                Ok(0) => return Err(unexpected(port, &reply)),
                Ok(_) => reply.push(byte),
                Err(cause)
                    if matches!(
                        cause.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(timed_out);
                }
                Err(cause) => return Err(DialError::Io(cause)),
            }
        }
        if relay::accepted(&reply).is_none() {
            return Err(unexpected(port, &reply));
        }
        stream.set_read_timeout(None).map_err(DialError::Io)?;
        stream.set_write_timeout(None).map_err(DialError::Io)?;
        Ok(OwnedSocket::from(stream))
    }

    fn console(&self) -> GuestConsole {
        self.console.as_ref().map_or_else(
            || GuestConsole::Kept {
                log: None,
                tail: Vec::new(),
            },
            kept,
        )
    }

    /// # Errors
    /// Returns [`Error::ForcedStop`] if the guest had to be stopped for, or how it ended otherwise.
    fn shutdown(mut self: Box<Self>) -> Result<(), Error> {
        self.stop()
    }
}

impl Drop for Guest {
    /// A caller that forgets, or unwinds, does not leave a guest running.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn unexpected(port: u32, reply: &[u8]) -> DialError {
    DialError::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "OpenVMM answered `CONNECT {port}` with \u{201c}{}\u{201d} rather than `OK` and a \
             connection id",
            String::from_utf8_lossy(reply).trim_end()
        ),
    ))
}

/// WHP refuses a machine with more processors than the host.
fn clamp_cpus(vcpus: u8) -> u32 {
    let host = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    u32::from(vcpus).clamp(1, u32::try_from(host).unwrap_or(u32::MAX))
}
