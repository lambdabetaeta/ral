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
//! - [`open_for_client`] answers "may the person on the other end of this
//!   pipe use this folder as they ask — read it, or change it" by *becoming*
//!   them for the length of the question, and keeps the folder they opened
//!   pinned for as long as the machine serves it ([`Granted`]);
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
use std::os::windows::ffi::{OsStrExt, OsStringExt};
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
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileAttributeTagInfo,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, OPEN_EXISTING, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use super::{PIPE, Reply, Request, VERSION, frame};
use crate::{BootArtifact, Error, Hypervisor, Machine, MachineSpec};

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
    // The client's own handle on the granted folder, held for as long as the
    // machine that serves it. Declared before `machine` so it is dropped
    // after it: the folder stays pinned until the machine is gone.
    let mut pinned: Option<Granted> = None;
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
                        pinned = Some(booted.folder);
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
            Request::Console => {
                // Asked of a machine that has usually already died, so
                // "there is no machine" is an ordinary answer here and not a
                // refusal: an empty console is what the client will write
                // down, and it says so in its own words.
                let console = machine
                    .as_ref()
                    .map(|machine| machine.console())
                    .unwrap_or_default();
                Reply::Console {
                    log: console.log,
                    tail: console.tail,
                }
            }
            Request::Stop => {
                // The broker's own handles go first if the client never
                // adopted them, so the guest sees both wires close either way.
                drop(wires.take());
                let outcome = match machine.take() {
                    Some(machine) => machine.shutdown().map_err(|e| e.to_string()),
                    None => Ok(()),
                };
                // Only now that the machine is gone is the folder let go.
                drop(pinned.take());
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
                 service speaks {VERSION}) — ask whoever administers this computer to install them together"
            ))
        }
    }
}

/// One booted machine, as the serving thread has to hold it: the machine
/// itself, the client's handle on the folder it serves, where its workspace
/// is, the two wires the broker still owns, and the description of each made
/// for the client.
struct Booted {
    machine: Box<dyn Machine>,
    folder: Granted,
    workspace: PathBuf,
    wires: crate::Wires,
    control_description: Vec<u8>,
    net_description: Vec<u8>,
}

/// How a refused boot is worded for the client: the backend's own reason, and
/// the folder it was asked about, spelled plainly.
///
/// Only what this side knows and the client does not may travel, and the
/// hypervisor's name is not it.  [`Error::Unavailable`] renders as "Hyper-V
/// could not start a machine: …", and `client::Brokered` wraps whatever arrives
/// here in an `Unavailable` of its own — it has to, since a refusal that never
/// reached a backend must still name the machine layer that refused.  So
/// sending the rendered error said the same sentence twice, either side of a
/// third "could not start a machine for …" added here: one failure wearing
/// three subjects, which is what a person actually read off a stale guest
/// image.  The reason alone crosses the pipe, and the client supplies the
/// subject exactly once.
fn refused_over(folder: &Path) -> impl FnOnce(Error) -> String {
    // Verbatim (`\\?\C:\…`) is how a canonicalised path arrives from a client,
    // and it is a convention of the Win32 call layer rather than anything a
    // reader of an error message should be shown.
    let folder = crate::hcs::plain(folder.to_path_buf());
    move |error| match error {
        Error::Unavailable { why, .. } => {
            format!("{why} (the folder asked for was {})", folder.display())
        }
        // Every other variant is already a sentence about the request, and each
        // one names the path it is about.
        named => named.to_string(),
    }
}

/// Boot one machine for the client on `pipe`, and describe its two wires for
/// that client's process.
///
/// Every check the broker makes is here, in order: the folder must be one the
/// *caller* may use in the way the grant asks, read-only or with changes
/// allowed, and it is the folder the caller's own handle reached rather than
/// the name they sent; the media is this installation's; the spec is
/// constructed, never received.
fn boot_for(pipe: HANDLE, folder: &Path, read_only: bool) -> Result<Booted, String> {
    // SAFETY: `pipe` is the connected server end this thread owns.
    let granted = unsafe { open_for_client(pipe, folder, read_only) }?;

    let artifact = media()?;
    // The path the client's own handle resolved to, never the string they
    // sent. [`Granted`] is why it keeps naming the same folder while the
    // compute service, running as itself, opens it by name.
    let mut spec = MachineSpec::for_folder(granted.path());
    spec.workspace.read_only = read_only;

    let hypervisor = crate::hcs::Hyperv::new(artifact, cache());
    let mut machine = hypervisor
        .boot(&spec)
        .map_err(refused_over(granted.path()))?;
    // `boot` resolved the path once more, as this service, and the share was
    // made from that answer. The one change the held handle cannot prevent is
    // an empty folder being turned into a link, so it is asked again here; a
    // refusal drops `machine`, which stops it.
    granted.unchanged()?;
    let workspace = machine.workspace_path().to_path_buf();

    let client = client_process(pipe)?;
    let wires = machine.take_wires();
    let control_description = describe_socket(wires.control.as_socket(), client)?;
    let net_description = describe_socket(wires.net.as_socket(), client)?;
    Ok(Booted {
        machine,
        folder: granted,
        workspace,
        wires,
        control_description,
        net_description,
    })
}

/// `IsReparseTagNameSurrogate`: the bit a reparse tag carries when the point
/// stands for *another name* — a junction, a symbolic link — rather than for
/// data kept somewhere unusual, as a cloud placeholder or a deduplicated file
/// does.
const REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;

/// The rights the client must hold on the folder itself for a grant.
///
/// Read-only asks for the folder to be listed and passed through, and for its
/// attributes to be read, which the check does straight afterwards
/// ([`Granted::open`]). With changes allowed it adds creating a file and
/// creating a folder in it: the two rights that let a person put something new
/// there themselves, and the two a standard user lacks on `C:\Program Files`,
/// `C:\Windows` and the root of the system drive.
///
/// `FILE_DELETE_CHILD` is left out deliberately. *Modify*, the grant most
/// shared folders carry, does not include it — someone with Modify deletes a
/// file by their right on the file — so asking for it would refuse the
/// commonest writable folder there is while proving nothing more about who
/// may write.
const fn rights_for(read_only: bool) -> u32 {
    let read = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES;
    if read_only {
        read
    } else {
        read | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY
    }
}

/// A folder the client has proved they may use, held open as them.
///
/// The handle is the proof and the pin at once. It was opened under the
/// client's own token asking for exactly the rights the grant needs
/// ([`rights_for`]), so that it opened at all is Windows' own access check,
/// made against the folder's real security descriptor rather than inferred
/// from a listing. It was opened *as* the folder, never through it — a
/// junction or symbolic link in the last place is refused rather than
/// followed — and the path it answers with ([`Granted::path`]) is the one
/// Windows resolved every earlier link to, under the client's token too.
///
/// It is held without `FILE_SHARE_DELETE` for as long as the machine lives.
/// While it is open the folder cannot be renamed or deleted, and neither can
/// any folder above it, so the path keeps naming this folder for the compute
/// service — which opens it by name, as itself, after the check is over.
pub struct Granted {
    handle: OwnedHandle,
    path: PathBuf,
}

impl Granted {
    /// Open `folder` for a grant as whichever token this thread holds.
    ///
    /// Only [`open_for_client`] calls this in the service, with the client's
    /// token on the thread; tests call it directly, as themselves.
    fn open(folder: &Path, read_only: bool) -> Result<Self, String> {
        let shown = crate::hcs::plain(folder.to_path_buf());
        let wide: Vec<u16> = folder.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated wide string alive for the call; no
        // security attributes and no template are passed. Backup semantics is
        // what lets `CreateFileW` open a directory at all, and grants nothing
        // more unless the token has the backup privilege enabled, which a
        // standard user's does not.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                rights_for(read_only),
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            let cause = io::Error::last_os_error();
            return Err(if read_only {
                format!(
                    "the folder {} cannot be opened by the account that asked for it: {cause}",
                    shown.display()
                )
            } else {
                format!(
                    "the folder {} cannot be opened for changes by the account that asked for \
                     it, so it cannot be granted with changes allowed: {cause}",
                    shown.display()
                )
            });
        }
        // SAFETY: a fresh handle this function owns.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };

        let tag = attribute_tag(&handle).map_err(|cause| {
            format!(
                "synod's machine service could not read what kind of thing {} is: {cause}",
                shown.display()
            )
        })?;
        judge(&shown, tag)?;
        let path = final_path(&handle).map_err(|cause| {
            format!(
                "synod's machine service could not tell where the folder {} really is: {cause}",
                shown.display()
            )
        })?;
        Ok(Self { handle, path })
    }

    /// Where the folder really is: the path Windows resolved it to, in the
    /// verbatim spelling `std::fs::canonicalize` also answers with.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the folder is still the real folder the client opened.
    ///
    /// The held handle stops the folder and everything above it from being
    /// renamed or deleted, but not an *empty* folder from being made into a
    /// junction in place, and a junction is followed by anything that opens
    /// the path by name. So once the compute service has done that, the folder
    /// is asked again.
    ///
    /// # Errors
    /// Returns the sentence to show the person who granted the folder.
    fn unchanged(&self) -> Result<(), String> {
        let shown = crate::hcs::plain(self.path.clone());
        let moved = || {
            format!(
                "the folder {} was changed into a link to somewhere else while its machine was \
                 starting, so the machine was stopped",
                shown.display()
            )
        };
        let tag = attribute_tag(&self.handle).map_err(|_| moved())?;
        judge(&shown, tag).map_err(|_| moved())?;
        match final_path(&self.handle) {
            Ok(now) if now == self.path => Ok(()),
            _ => Err(moved()),
        }
    }
}

/// Whether the client on the other end of `pipe` may use `folder` as the grant
/// asks, answered by becoming them, and the folder held open as them if so.
///
/// `ImpersonateNamedPipeClient` puts the caller's token on this thread, the
/// folder is opened under it ([`Granted`] says how and why), and the token is
/// dropped again. Anything less — checking as `LocalSystem`, which can read and
/// write nearly everything — would let one user have another user's documents
/// mounted into their own guest, or have this service's own program handed to
/// a guest to change, which is worse again.
///
/// A client that connected at the *identification* level, which lets the
/// service learn who they are but not act as them, is refused here too: the
/// open fails, and a failed open is never waved through.
///
/// # Errors
/// Returns the sentence to show the person who granted the folder: that it
/// cannot be opened as the grant asks, that it is a link rather than a folder,
/// or that the check itself could not be made.
///
/// # Safety
/// `pipe` must be a live, *connected* server end of a named pipe. Impersonation
/// is meaningless on anything else, and on a handle that is not a pipe at all
/// the platform is being asked to read a kind of object it was not given.
pub unsafe fn open_for_client(
    pipe: HANDLE,
    folder: &Path,
    read_only: bool,
) -> Result<Granted, String> {
    // SAFETY: `pipe` is a connected server-end handle; impersonation lasts until
    // `RevertToSelf`, which the guard below performs on every path out.
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(format!(
            "synod's machine service could not check who was asking: {}",
            io::Error::last_os_error()
        ));
    }
    let guard = Impersonation;
    let granted = Granted::open(folder, read_only);
    drop(guard);
    granted
}

/// Whether an object with these attributes may be granted as a folder.
///
/// A reparse point is refused only when it stands for another name — a
/// junction or a symbolic link. The object was opened as itself, so the access
/// check was made against the link and not against what it points at, and
/// whatever later opens the path by name would follow it somewhere the check
/// never looked. Other reparse points are the folder's own data kept in an
/// unusual way — a `OneDrive` folder whose files are online-only is one — and
/// refusing them would refuse an ordinary Documents folder.
fn judge(shown: &Path, tag: FILE_ATTRIBUTE_TAG_INFO) -> Result<(), String> {
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && tag.ReparseTag & REPARSE_TAG_NAME_SURROGATE != 0
    {
        return Err(format!(
            "{} is a link to another folder (a junction or a symbolic link), and synod's machine \
             service grants only a real folder: grant the folder it points to instead",
            shown.display()
        ));
    }
    if tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(format!("{} is not a folder", shown.display()));
    }
    Ok(())
}

/// The attributes and reparse tag of the object `handle` names, read from the
/// handle rather than by name.
fn attribute_tag(handle: &OwnedHandle) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    let mut tag = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: the handle is live and was opened with `FILE_READ_ATTRIBUTES`;
    // `tag` is a writable structure of exactly the type and size this
    // information class fills in.
    let read = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut tag).cast(),
            u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).expect("a small structure"),
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(tag)
}

/// The path of the object `handle` names, with every link on the way to it
/// resolved: `\\?\C:\…`, or `\\?\UNC\server\share\…` on a file share.
fn final_path(handle: &OwnedHandle) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: the handle is live, and `buffer` is writable for `capacity`
        // wide characters.
        let written = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        let written = usize::try_from(written).expect("a u32 fits a usize here");
        if written < buffer.len() {
            // It fitted: `written` excludes the terminating NUL.
            buffer.truncate(written);
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)));
        }
        // Too small: `written` is the size needed, NUL included.
        buffer.resize(written, 0);
    }
}

/// Reverts impersonation on every path out of [`open_for_client`], including
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

    /// A read-only grant asks to list and pass through the folder and nothing
    /// that changes it; a grant with changes allowed adds exactly the two
    /// rights that let a person put something new there. Neither asks for more
    /// than it proves — in particular not `FILE_DELETE_CHILD`, which *Modify*
    /// lacks.
    #[test]
    fn each_grant_asks_for_the_rights_it_needs_and_no_more() {
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES, WRITE_DAC,
        };
        let read = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES;
        let add = FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY;
        assert_eq!(rights_for(true), read);
        assert_eq!(rights_for(true) & add, 0, "read-only proves no write");
        assert_eq!(rights_for(false), read | add);
        for right in [DELETE, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES, WRITE_DAC] {
            assert_eq!(rights_for(false) & right, 0, "{right:#x} is not asked for");
        }
    }

    /// Junctions and symbolic links are refused, because the check was made on
    /// the link; a folder whose data is kept in an unusual way — a `OneDrive`
    /// placeholder — is not; and a file is not a folder.
    #[test]
    fn only_a_real_folder_is_judged_grantable() {
        const JUNCTION: u32 = 0xA000_0003;
        const SYMLINK: u32 = 0xA000_000C;
        const CLOUD: u32 = 0x9000_001A;
        let shown = Path::new(r"C:\Users\secretary\Documents");
        let tagged = |attributes, tag| FILE_ATTRIBUTE_TAG_INFO {
            FileAttributes: attributes,
            ReparseTag: tag,
        };
        let link = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT;
        for tag in [JUNCTION, SYMLINK] {
            let why = judge(shown, tagged(link, tag)).expect_err("a link is refused");
            assert!(why.contains("link to another folder"), "{why}");
            assert!(why.contains(r"C:\Users\secretary\Documents"), "{why}");
        }
        judge(shown, tagged(link, CLOUD)).expect("an online-only folder is a folder");
        judge(shown, tagged(FILE_ATTRIBUTE_DIRECTORY, 0)).expect("a plain folder");
        let why = judge(shown, tagged(0x80, 0)).expect_err("a file is refused");
        assert!(why.contains("is not a folder"), "{why}");
    }

    /// A folder this account made is granted either way, and the path handed
    /// on is the one Windows resolved, spelled as `canonicalize` spells it —
    /// so the compute service's own resolution, later, lands on it unchanged.
    #[test]
    fn a_folder_one_owns_is_granted_read_only_and_writable() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let real = std::fs::canonicalize(dir.path()).expect("canonical");
        for read_only in [true, false] {
            let granted = Granted::open(dir.path(), read_only).expect("granted");
            assert_eq!(granted.path(), real, "read_only = {read_only}");
            granted.unchanged().expect("nothing moved");
        }
    }

    /// A folder this account may read but not add to is granted read-only and
    /// refused with changes allowed — the check that list access alone never
    /// made.
    #[test]
    fn a_folder_one_may_only_read_is_refused_with_changes_allowed() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let restricted = Restricted::to(dir.path(), "D:P(A;OICI;FRFX;;;WD)");
        Granted::open(dir.path(), true).expect("readable, so granted read-only");
        let why = Granted::open(dir.path(), false)
            .err()
            .expect("not writable, so refused with changes allowed");
        assert!(why.contains("cannot be opened for changes"), "{why}");
        drop(restricted);
    }

    /// A folder this account may not even list is refused either way.
    #[test]
    fn a_folder_one_may_not_read_is_refused() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let restricted = Restricted::to(dir.path(), "D:P(A;OICI;RC;;;WD)");
        for read_only in [true, false] {
            assert!(Granted::open(dir.path(), read_only).is_err(), "{read_only}");
        }
        drop(restricted);
    }

    /// A junction is refused rather than followed, even to a folder this
    /// account could have granted directly: the rights checked were the
    /// link's, not its target's.
    #[test]
    fn a_junction_is_refused() {
        let target = tempfile::tempdir().expect("scratch folder");
        let holder = tempfile::tempdir().expect("scratch folder");
        let link = holder.path().join("away");
        if !junction(&link, target.path()) {
            eprintln!("mklink /J is not available here; skipping");
            return;
        }
        for read_only in [true, false] {
            let why = Granted::open(&link, read_only)
                .err()
                .expect("a junction is refused");
            assert!(why.contains("link to another folder"), "{why}");
        }
    }

    /// A junction *above* the folder is resolved, under the same token, and the
    /// machine is handed the real path — so no later resolution by name can
    /// land anywhere the check did not.
    #[test]
    fn a_folder_reached_through_a_junction_is_granted_as_itself() {
        let target = tempfile::tempdir().expect("scratch folder");
        std::fs::create_dir(target.path().join("inner")).expect("inner folder");
        let holder = tempfile::tempdir().expect("scratch folder");
        let link = holder.path().join("away");
        if !junction(&link, target.path()) {
            eprintln!("mklink /J is not available here; skipping");
            return;
        }
        let granted = Granted::open(&link.join("inner"), false).expect("granted");
        let real = std::fs::canonicalize(target.path().join("inner")).expect("canonical");
        assert_eq!(granted.path(), real);
    }

    /// While the grant is held, neither the folder nor the folder above it can
    /// be renamed — which is what keeps its path naming it for the compute
    /// service — and once it is dropped, both can.
    #[test]
    fn a_held_folder_cannot_be_moved_from_under_its_path() {
        let root = tempfile::tempdir().expect("scratch folder");
        let parent = root.path().join("parent");
        let folder = parent.join("folder");
        std::fs::create_dir_all(&folder).expect("folders");

        let granted = Granted::open(&folder, true).expect("granted");
        assert!(
            std::fs::rename(&folder, parent.join("moved")).is_err(),
            "the folder itself is pinned"
        );
        assert!(
            std::fs::rename(&parent, root.path().join("moved")).is_err(),
            "and so is the folder above it"
        );
        drop(granted);
        std::fs::rename(&folder, parent.join("moved")).expect("free once released");
    }

    /// A file is not a folder, and is refused as one.
    #[test]
    fn a_file_is_refused() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"hello").expect("a file");
        let why = Granted::open(&file, true).err().expect("refused");
        assert!(why.contains("is not a folder"), "{why}");
    }

    /// Make a junction at `link` pointing at `target`, which needs no
    /// privilege, unlike a symbolic link. `false` where `cmd` cannot.
    fn junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// A folder whose DACL is replaced for the length of a test, and handed
    /// back afterwards so the temporary directory can be deleted. The owner
    /// keeps the right to rewrite the DACL whatever it says.
    struct Restricted(PathBuf);

    impl Restricted {
        fn to(folder: &Path, sddl: &str) -> Self {
            set_dacl(folder, sddl);
            Self(folder.to_path_buf())
        }
    }

    impl Drop for Restricted {
        fn drop(&mut self) {
            set_dacl(&self.0, "D:P(A;OICI;FA;;;WD)");
        }
    }

    fn set_dacl(folder: &Path, sddl: &str) {
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, SetFileSecurityW};
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and alive; `descriptor` is a
        // writable slot the call fills with a `LocalAlloc`'d descriptor.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0, "{}", io::Error::last_os_error());
        let path: Vec<u16> = folder.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `path` is NUL-terminated and `descriptor` a valid descriptor.
        let set = unsafe { SetFileSecurityW(path.as_ptr(), DACL_SECURITY_INFORMATION, descriptor) };
        let cause = io::Error::last_os_error();
        // SAFETY: allocated by the conversion above and not used after this.
        unsafe { LocalFree(descriptor.cast()) };
        assert_ne!(set, 0, "{cause}");
    }
}
