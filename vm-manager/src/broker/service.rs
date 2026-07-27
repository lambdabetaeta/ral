//! The broker's own side: the only privileged code synod ships.
//!
//! Read [`super`] first — it carries the argument for why this exists and what
//! makes it safe. This module is the implementation of that argument, and every
//! function in it is one of the checks the argument promised:
//!
//! - [`serve`] listens on a pipe whose security descriptor decides who may
//!   speak to a privileged service at all ([`PIPE_SDDL`]);
//! - [`media`] reads the boot artifact from *this executable's own* directory,
//!   so no caller can name a kernel;
//! - [`readable_by_client`] answers "may the person on the other end of this
//!   pipe read this folder" by *becoming* them for the length of the question;
//! - [`serve_client`] holds one machine for one connection and drops it when the
//!   connection ends, so a client that dies cannot leave a machine running.
//!
//! # What a reviewer should check here
//!
//! That the machine's document is never influenced by the request beyond the
//! folder and the read-only flag. Everything else — the media, the devices, the
//! absence of a network adapter, the cache the disks are made in — is
//! constructed below from this process's own state. If a future request field
//! ever reaches [`crate::hcs`] without passing a check in this file, the
//! argument in [`super`] stops being true.

use std::io;
use std::os::windows::io::{
    AsRawHandle, AsRawSocket, AsSocket, BorrowedSocket, FromRawHandle, IntoRawHandle, OwnedHandle,
};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{RevertToSelf, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use super::{PIPE, Reply, Request, VERSION, frame};
use crate::{BootArtifact, Hypervisor, Machine, MachineSpec};

/// Who may talk to the broker.
///
/// A protected DACL (`D:P`, so nothing inherited widens it) granting `SYSTEM`
/// and the built-in Administrators everything, and *interactively logged-on
/// users* read and write. `IU` is the deliberate choice over the more obvious
/// `AU` (authenticated users): it is satisfied only by a session someone is
/// actually sitting at, which excludes a network logon and excludes other
/// services. Synod is a desktop application; nothing else has business asking
/// for a virtual machine.
pub const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

/// `PIPE_ACCESS_DUPLEX`.
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;

/// The pipe's buffers. The protocol's largest message is a socket description
/// of a few hundred bytes.
const PIPE_BUFFER: u32 = 4 * 1024;

/// Where the broker keeps the disks it makes: machine-wide state, under
/// `%ProgramData%`, not one user's profile.
///
/// The service runs as `LocalSystem`, so `%LOCALAPPDATA%` would resolve into
/// `SYSTEM`'s own profile — technically writable and semantically wrong. The
/// wrapped rootfs is identical for every user of the computer, so it is written
/// once, here, rather than once per profile.
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: lifting one path-valued environment variable for the service's own \
              machine-wide state directory; no shell, no run, no card."
)]
pub fn cache() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("Synod")
        .join("Machine")
}

/// The boot media installed beside this executable.
///
/// The one place the machine's image comes from, and the reason a request can
/// never name a kernel: the installer puts `synod-machine-broker.exe` and
/// `boot\` in the same directory, so this is a property of the installation
/// rather than of the conversation.
///
/// # The rootfs arrives compressed, and the format leaves no choice
///
/// A Windows Installer cabinet cannot hold a file of two gigabytes: `light`
/// refuses to link one at all — *"is too large, file size must be less than
/// 2147483648"* — and the rootfs is two and a half. So the MSI ships the same
/// `rootfs.img.zst` the macOS bundle does, and the compressed spelling is
/// preferred here over the plain one.
///
/// Inflating it is not this function's business, nor a second copy on disk:
/// `hcs::vhd::ensure_rootfs_vhd` decompresses straight into the VHD it was going
/// to write regardless, in the cache under `%ProgramData%` that this service
/// owns.  That cache is what makes the arrangement fit a `LocalSystem` service
/// at all — a per-*user* cache is precisely what it must not write into, and
/// here there is no user to have one.  One inflate, on the first boot after an
/// installation, shared by every session on the computer afterwards.
///
/// # Errors
/// Returns a sentence if this executable's own location cannot be read.
pub fn media() -> Result<BootArtifact, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("the machine service could not find its own program: {e}"))?;
    let beside = exe
        .parent()
        .ok_or_else(|| "the machine service is not installed in a directory".to_string())?;

    let installed = artifact(&beside.join("boot"));
    if installed.kernel.is_file() {
        return Ok(installed);
    }
    // Not installed, so this is a service registered out of a checkout — what
    // `just broker-install` does, and the only way the privileged half can be
    // developed without building an MSI first. The image pipeline's own output
    // stands in for the `boot\` directory an installation would have beside it.
    // Deliberately a *fallback*: on a user's computer the branch above always
    // wins, so a checkout on the same machine can never redirect the service to
    // media somebody happens to have lying about.
    let mut cursor: &Path = beside;
    while let Some(parent) = cursor.parent() {
        if parent.file_name().is_some_and(|name| name == "target") {
            let out = parent
                .parent()
                .ok_or_else(|| "this checkout has no workspace root".to_string())?
                .join("vm-image")
                .join("out");
            let development = BootArtifact {
                rootfs: rootfs_in(&out),
                ..artifact(&out.join("boot"))
            };
            if development.kernel.is_file() {
                return Ok(development);
            }
            break;
        }
        cursor = parent;
    }
    Err(format!(
        "the machine service found no guest image: it looked beside itself, in {}, and for the \
         image pipeline's own output if this is a checkout. An installation puts the image there; \
         a checkout builds it with `just guest-boot amd64` and `just guest-rootfs amd64`",
        beside.join("boot").display()
    ))
}

/// The three files of a boot artifact, as they are named inside a `boot`
/// directory. One spelling, used by both the installed and the development
/// layout, so the two cannot drift apart.
///
fn artifact(boot: &Path) -> BootArtifact {
    BootArtifact {
        kernel: boot.join("kernel"),
        initramfs: boot.join("initramfs.img"),
        rootfs: rootfs_in(boot),
    }
}

/// The rootfs inside `dir`, in whichever of its two spellings is there.
///
/// The plain image wins when it exists and the archive is the fallback, which is
/// the right way round for both layouts: an installation has only
/// `rootfs.img.zst`, while a checkout that has built the image holds *both*, and
/// preferring the archive would make it inflate two and a half gigabytes it
/// already has lying there uncompressed.
fn rootfs_in(dir: &Path) -> PathBuf {
    let plain = dir.join("rootfs.img");
    if plain.is_file() {
        plain
    } else {
        dir.join("rootfs.img.zst")
    }
}

/// Serve until the process is stopped.
///
/// One instance of the pipe is created at a time and handed to a thread once a
/// client connects, with the next instance created immediately afterwards. A
/// client that arrives in the gap between the two is told the pipe is busy and
/// retries, which its own connect already does.
///
/// # Errors
/// Returns the first error that leaves the broker unable to listen at all — a
/// pipe it cannot create. A failure serving one client is that client's, and is
/// answered on its own connection.
pub fn serve() -> io::Result<()> {
    let mut first = true;
    loop {
        let pipe = create_instance(first)?;
        first = false;
        // SAFETY: `pipe` is a live server-end handle; a null overlapped pointer
        // asks for the blocking form, which is what a dedicated accept loop
        // wants.
        let connected = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) };
        if connected == 0
            && io::Error::last_os_error().raw_os_error() != Some(ERROR_PIPE_CONNECTED.cast_signed())
        {
            // This client's connection failed, not the broker's ability to
            // listen: drop it and take the next one.
            continue;
        }
        std::thread::Builder::new()
            .name("synod-broker-client".to_string())
            .spawn(move || serve_client(pipe))
            .ok();
    }
}

/// Serve one client, for as long as it stays connected.
///
/// The machine this connection owns lives in a local: when this function
/// returns — a stop, a hangup, a protocol fault, a panic — the machine is
/// dropped and torn down with its session disk. That is the whole lifetime
/// story, and it needs no table of live machines to keep honest.
fn serve_client(pipe: OwnedHandle) {
    let handle = pipe.as_raw_handle();
    // SAFETY: the handle is a duplex pipe instance this thread owns; a `File`
    // is std's reader/writer over exactly such a handle, and takes ownership so
    // it is closed once, on return.
    let mut stream = unsafe { std::fs::File::from_raw_handle(pipe.into_raw_handle()) };
    let mut machine: Option<Box<dyn Machine>> = None;
    // The broker's own handles on the two wires, held only until the client
    // says it has made its own ([`Request::Adopted`]).  One `Option` around
    // both, not two around each, so letting go is the one statement below
    // rather than something to remember to do twice.
    let mut wires: Option<crate::Wires> = None;

    loop {
        // The client has gone; the machine goes with it.
        let Ok(Some(request)) = frame::read::<_, Request>(&mut stream) else {
            return;
        };
        let reply = match request {
            Request::Boot {
                version,
                folder,
                read_only,
            } => if machine.is_some() {
                Reply::Refused("this connection already has a machine".to_string())
            } else {
                match boot_for(handle, &folder, read_only) {
                    Ok(booted) => {
                        machine = Some(booted.machine);
                        wires = Some(booted.wires);
                        Reply::Booted {
                            workspace: booted.workspace,
                            control: booted.control_description,
                            net: booted.net_description,
                        }
                    }
                    Err(why) => Reply::Refused(why),
                }
            }
            .versioned(version),
            Request::Adopted => {
                // The client has its own sockets now, so the broker's must
                // go: while they lived, the guest could not see the
                // end-of-file a closing client is supposed to cause.
                drop(wires.take());
                continue;
            }
            Request::Stop => {
                // The broker's own handles go first if the client never
                // adopted them, so the guest sees both wires close either way.
                drop(wires.take());
                let outcome = match machine.take() {
                    Some(machine) => machine.shutdown().map_err(|e| e.to_string()),
                    None => Ok(()),
                };
                let _ = frame::write(&mut stream, &Reply::Stopped(outcome));
                return;
            }
        };
        if frame::write(&mut stream, &reply).is_err() {
            return;
        }
    }
}

/// A reply, refused instead if the client speaks another version of the
/// protocol.
trait Versioned {
    fn versioned(self, spoken: u32) -> Self;
}

impl Versioned for Reply {
    fn versioned(self, spoken: u32) -> Self {
        if spoken == VERSION {
            self
        } else {
            Self::Refused(format!(
                "synod and its machine service are different versions (synod speaks {spoken}, the \
                 service speaks {VERSION}) — ask your IT department to install them together"
            ))
        }
    }
}

/// One booted machine, as the serving thread has to hold it: the machine
/// itself, where its workspace is, the two wires the broker still owns, and
/// the description of each made for the client.
struct Booted {
    machine: Box<dyn Machine>,
    workspace: PathBuf,
    wires: crate::Wires,
    control_description: Vec<u8>,
    net_description: Vec<u8>,
}

/// Boot one machine for the client on `pipe`, and describe its two wires for
/// that client's process.
///
/// Every check the broker makes is here, in order: the folder must be one the
/// *caller* can read; the media is this installation's; the spec is constructed,
/// never received.
fn boot_for(pipe: HANDLE, folder: &Path, read_only: bool) -> Result<Booted, String> {
    // SAFETY: `pipe` is the connected server end this thread owns.
    unsafe { readable_by_client(pipe, folder) }?;

    let artifact = media()?;
    let mut spec = MachineSpec::for_folder(folder);
    spec.workspace.read_only = read_only;

    let hypervisor = crate::hcs::Hyperv::new(artifact, cache());
    let mut machine = hypervisor
        .boot(&spec)
        .map_err(|e| format!("could not start a machine for {}: {e}", folder.display()))?;
    let workspace = machine.workspace_path().to_path_buf();

    let client = client_process(pipe)?;
    let wires = machine.take_wires();
    let control_description = describe_socket(wires.control.as_socket(), client)?;
    let net_description = describe_socket(wires.net.as_socket(), client)?;
    Ok(Booted {
        machine,
        workspace,
        wires,
        control_description,
        net_description,
    })
}

/// Whether the client on the other end of `pipe` can read `folder`.
///
/// Answered by becoming them: `ImpersonateNamedPipeClient` puts the caller's
/// token on this thread, the folder is opened under it, and the token is dropped
/// again. Anything less — checking as `LocalSystem`, which can read everything —
/// would let one user have another user's documents mounted into their own
/// guest, which is a worse hole than the one this service exists to close.
///
/// # Errors
/// Returns the sentence to show the person who granted the folder: that it
/// cannot be read, or that the check itself could not be made (which is refused,
/// never waved through).
///
/// # Safety
/// `pipe` must be a live, *connected* server end of a named pipe. Impersonation
/// is meaningless on anything else, and on a handle that is not a pipe at all
/// the platform is being asked to read a kind of object it was not given.
pub unsafe fn readable_by_client(pipe: HANDLE, folder: &Path) -> Result<(), String> {
    // SAFETY: `pipe` is a connected server-end handle; impersonation lasts until
    // `RevertToSelf`, which the guard below performs on every path out.
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(format!(
            "synod's machine service could not check who was asking: {}",
            io::Error::last_os_error()
        ));
    }
    let guard = Impersonation;
    #[allow(
        clippy::disallowed_methods,
        reason = "REASONED-SILENT: the access check itself — reading the granted folder as the \
                  caller. Host-side, before any machine exists; no shell, no run, no card."
    )]
    let readable = std::fs::read_dir(folder);
    drop(guard);

    match readable {
        Ok(_) => Ok(()),
        Err(cause) => Err(format!(
            "the folder {} cannot be opened by the account that asked for it: {cause}",
            folder.display()
        )),
    }
}

/// Reverts impersonation on every path out of [`readable_by_client`], including
/// an unwind — a thread left wearing a client's token would answer the *next*
/// client's questions as this one.
struct Impersonation;

impl Drop for Impersonation {
    fn drop(&mut self) {
        // SAFETY: called exactly once, on a thread this module impersonated.
        unsafe { RevertToSelf() };
    }
}

/// The process id of the client on `pipe`, from the kernel rather than from
/// anything the client said.
fn client_process(pipe: HANDLE) -> Result<u32, String> {
    let mut pid = 0u32;
    // SAFETY: `pipe` is a connected server-end handle and `pid` a writable slot.
    if unsafe { GetNamedPipeClientProcessId(pipe, &raw mut pid) } == 0 {
        return Err(format!(
            "synod's machine service could not identify the program that asked: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(pid)
}

/// Describe `socket` for process `pid`, so that process can make its own.
///
/// The description is valid for that one process and nothing else, which is what
/// makes handing it over safe even though it travels as ordinary bytes.
fn describe_socket(socket: BorrowedSocket<'_>, pid: u32) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Networking::WinSock::{SOCKET_ERROR, WSAPROTOCOL_INFOW};

    // SAFETY: `WSAPROTOCOL_INFOW` is a plain C structure of `Copy` fields, so a
    // zeroed one is a valid value to be written into.
    let mut info = unsafe { std::mem::zeroed::<WSAPROTOCOL_INFOW>() };
    // SAFETY: the socket is live for this call, `pid` names the process the
    // duplicate is for, and `info` is a writable structure of the type the call
    // fills in.
    let duplicated = unsafe {
        windows_sys::Win32::Networking::WinSock::WSADuplicateSocketW(
            usize::try_from(socket.as_raw_socket()).expect("a SOCKET is pointer-sized"),
            pid,
            &raw mut info,
        )
    };
    if duplicated == SOCKET_ERROR {
        return Err(format!(
            "the machine's control plane could not be handed to synod: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: `info` is a fully initialised `WSAPROTOCOL_INFOW` with no pointers
    // or padding invariants, so its bytes are a faithful description of it.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&raw const info).cast::<u8>(),
            size_of::<WSAPROTOCOL_INFOW>(),
        )
    };
    Ok(bytes.to_vec())
}

/// One instance of the pipe, with the descriptor that decides who may connect.
fn create_instance(first: bool) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_FIRST_PIPE_INSTANCE;

    let sddl: Vec<u16> = PIPE_SDDL.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: `sddl` is a NUL-terminated wide string alive for the call, and
    // `descriptor` a writable slot the call fills with a `LocalAlloc`'d
    // descriptor; the optional size out-parameter is not wanted.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("a small structure"),
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };

    let name: Vec<u16> = PIPE.encode_utf16().chain(Some(0)).collect();
    let mode = if first {
        // Only on the first instance, and deliberately: it is what makes a
        // second broker fail to start rather than quietly serve half the
        // clients on the same name.
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    // SAFETY: both wide strings and `attributes` are alive for the call.
    let pipe = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            &raw const attributes,
        )
    };
    // SAFETY: the descriptor was `LocalAlloc`'d by the conversion above and is
    // not referenced after `CreateNamedPipeW` has copied what it needs.
    unsafe { LocalFree(descriptor.cast()) };

    if pipe == INVALID_HANDLE_VALUE || pipe.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle this function owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(pipe) })
}

/// Close a raw handle, for the two paths that hold one outside an `OwnedHandle`.
#[allow(dead_code, reason = "kept beside the FFI it belongs to")]
fn close(handle: HANDLE) {
    // SAFETY: called once per handle, on handles this module owns.
    unsafe { CloseHandle(handle) };
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: test scaffolding names literal paths and reads the cache location \
              back; no shell, no run, no card."
)]
mod tests {
    use super::*;

    /// The descriptor grants three principals and no more, and reaches
    /// interactive sessions rather than every authenticated identity on the
    /// network — the one detail that decides who can speak to a privileged
    /// service.
    #[test]
    fn the_pipes_descriptor_admits_only_a_person_at_this_computer() {
        assert!(PIPE_SDDL.starts_with("D:P"), "a protected DACL");
        assert!(PIPE_SDDL.contains(";IU)"), "interactive users");
        assert!(
            !PIPE_SDDL.contains(";AU)"),
            "not every authenticated identity"
        );
        assert!(!PIPE_SDDL.contains(";WD)"), "not everyone");
        assert!(!PIPE_SDDL.contains(";AN)"), "not anonymous");
    }

    /// The descriptor is one Windows will actually parse — a typo here would
    /// otherwise surface as a service that starts and then refuses every
    /// client.
    #[test]
    fn the_descriptor_parses() {
        let pipe = create_instance(false).expect("a pipe instance");
        drop(pipe);
    }

    /// The media is named relative to the broker's own program — beside it when
    /// installed, and otherwise the image pipeline's output in the checkout the
    /// program was built in. Never anything a request could influence, which is
    /// the property the whole narrow-surface argument rests on.
    ///
    /// Which of the two answers this test gets depends on where it is run from,
    /// and both are correct; what it pins is that the answer is *derived from
    /// this executable's location*, and that a missing image is named rather
    /// than guessed at.
    #[test]
    fn the_media_is_derived_from_the_brokers_own_location() {
        let exe = std::env::current_exe().unwrap();
        let beside = exe.parent().unwrap().join("boot");
        match media() {
            Ok(found) => {
                assert!(found.kernel.is_file(), "an answer names a real kernel");
                assert!(
                    found.kernel.starts_with(beside.parent().unwrap())
                        || found
                            .kernel
                            .components()
                            .any(|c| c.as_os_str() == "vm-image"),
                    "the media must come from this program's own tree: {:?}",
                    found.kernel
                );
                assert_eq!(found.initramfs.file_name().unwrap(), "initramfs.img");
                assert_eq!(found.rootfs.file_name().unwrap(), "rootfs.img");
            }
            Err(why) => {
                assert!(why.contains("no guest image"), "{why}");
                assert!(
                    why.contains("just guest-boot"),
                    "and says how to get one: {why}"
                );
            }
        }
    }

    /// The disks live in machine-wide state, not in the profile of whichever
    /// account the service happens to run as.
    #[test]
    fn the_cache_is_machine_wide() {
        let cache = cache();
        assert!(
            cache.ends_with(Path::new("Synod").join("Machine")),
            "{cache:?}"
        );
        if let Some(program_data) = std::env::var_os("ProgramData") {
            assert!(cache.starts_with(PathBuf::from(program_data)));
        }
    }

    /// A client of another version is refused with both versions named, rather
    /// than being served a protocol it does not speak.
    #[test]
    fn a_version_mismatch_is_refused_by_name() {
        let refused = Reply::Stopped(Ok(())).versioned(VERSION + 1);
        match refused {
            Reply::Refused(why) => {
                assert!(why.contains(&(VERSION + 1).to_string()), "{why}");
                assert!(why.contains(&VERSION.to_string()), "{why}");
            }
            other => panic!("a mismatch must be refused: {other:?}"),
        }
        assert!(matches!(
            Reply::Stopped(Ok(())).versioned(VERSION),
            Reply::Stopped(Ok(()))
        ));
    }
}
