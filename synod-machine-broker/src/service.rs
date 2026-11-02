//! Only the folder and read-only flag may influence the privileged machine spec.
//! One pipe connection owns one machine; the service supplies media and devices.

use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{
    AsRawHandle, AsRawSocket, AsSocket, BorrowedSocket, FromRawHandle, IntoRawHandle, OwnedHandle,
};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
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

use vm_manager::broker::wire::{PIPE, Reply, Request, VERSION, frame};
use vm_manager::hcs::Hyperv;
use vm_manager::hcs::win::{HYPERVISOR, document_text, plain, wide};
use vm_manager::media;
use vm_manager::{Booted, Error, GuestConsole, Hypervisor, Machine, MachineSpec, Wires};

use crate::site::{Silent, site};

/// Protected DACL: SYSTEM, Administrators and interactive users only.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;

const PIPE_BUFFER: u32 = 4 * 1024;

/// The rootfs cache is shared machine-wide, outside any user's profile.
pub(crate) fn cache() -> PathBuf {
    site!(Silent::ProgramDataVar, std::env::var_os("ProgramData"))
        .map_or_else(
            || site!(Silent::ProgramDataFallback, std::env::temp_dir()),
            PathBuf::from,
        )
        .join("Synod")
        .join("Machine")
}

/// # Errors
/// Only pipe creation failure stops the listener; client failures are isolated.
pub(crate) fn serve() -> io::Result<()> {
    let mut first = true;
    loop {
        let pipe = create_instance(first)?;
        first = false;
        // SAFETY: `pipe` is a live server end; null selects blocking I/O.
        let connected = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) };
        if connected == 0
            && io::Error::last_os_error().raw_os_error() != Some(ERROR_PIPE_CONNECTED.cast_signed())
        {
            continue;
        }
        std::thread::Builder::new()
            .name("synod-broker-client".to_string())
            .spawn(move || serve_pipe(pipe))
            .ok();
    }
}

fn serve_pipe(pipe: OwnedHandle) {
    let handle = pipe.as_raw_handle();
    // SAFETY: `into_raw_handle` transfers sole ownership of this duplex pipe to `File`.
    let mut stream = unsafe { std::fs::File::from_raw_handle(pipe.into_raw_handle()) };
    serve_client(&mut stream, |folder, read_only| {
        boot_for(handle, folder, read_only)
    });
}

fn serve_client(
    stream: &mut (impl io::Read + io::Write),
    mut boot: impl FnMut(&Path, bool) -> Result<Lease, Error>,
) {
    // Drop order: wires, machine, then the pinned folder.
    let mut pinned: Option<Granted> = None;
    let mut machine: Option<Box<dyn Machine>> = None;
    // Both wires are held until `Adopted`, then released together.
    let mut wires: Option<Wires> = None;

    loop {
        let request = match Request::read(stream) {
            Ok(Some(Ok(request))) => request,
            Ok(Some(Err(refusal))) => {
                let _ = frame::write(stream, &Reply::Refused(refusal));
                return;
            }
            Ok(None) | Err(_) => return,
        };
        let reply = match request {
            Request::Boot {
                version,
                folder,
                read_only,
            } => {
                if version != VERSION {
                    Reply::Refused(Error::VersionSkew {
                        client: version,
                        service: VERSION,
                    })
                } else if machine.is_some() {
                    Reply::Refused(Error::backend(
                        HYPERVISOR,
                        "this connection already has a machine",
                    ))
                } else {
                    match boot(&folder, read_only) {
                        Ok(booted) => {
                            machine = Some(booted.machine);
                            pinned = Some(booted.folder);
                            wires = Some(booted.wires);
                            Reply::Booted {
                                control: booted.control_description,
                                net: booted.net_description,
                            }
                        }
                        Err(why) => Reply::Refused(why),
                    }
                }
            }
            Request::Adopted => {
                // Release the broker's sockets so client closure can reach the guest as EOF.
                drop(wires.take());
                continue;
            }
            Request::Console => Reply::Console(machine.as_ref().map_or_else(
                || GuestConsole::Kept {
                    log: None,
                    tail: Vec::new(),
                },
                |machine| machine.console(),
            )),
            Request::Stop => {
                // Unadopted wires close before shutdown so the guest sees EOF.
                drop(wires.take());
                let outcome = machine.take().map_or(Ok(()), Machine::shutdown);
                drop(pinned.take());
                let _ = frame::write(stream, &Reply::Stopped(outcome));
                return;
            }
        };
        if frame::write(stream, &reply).is_err() {
            return;
        }
    }
}

struct Lease {
    wires: Wires,
    machine: Box<dyn Machine>,
    folder: Granted,
    control_description: Vec<u8>,
    net_description: Vec<u8>,
}

fn boot_for(pipe: HANDLE, folder: &Path, read_only: bool) -> Result<Lease, Error> {
    // Refuse unspellable names before authorising a folder the HCS document cannot name.
    document_text(folder)?;
    // SAFETY: `pipe` is the connected server end this thread owns.
    let granted = unsafe { open_for_client(pipe, folder, read_only) }?;

    let exe = std::env::current_exe().map_err(|cause| Error::ServiceUnlocated {
        cause: cause.to_string(),
    })?;
    let artifact = media::locate(&exe)?.found().resolve()?;
    // Share the path resolved by the client's pinned handle.
    let spec = MachineSpec {
        folder: granted.path().into(),
        read_only,
    };

    let hypervisor = Hyperv::new(artifact, cache());
    let Booted { machine, wires } = hypervisor.boot(&spec)?;
    // A pinned empty folder can become a junction; recheck after HCS opens the share.
    granted.unchanged()?;

    let client = client_process(pipe)?;
    let control_description = describe_socket(wires.control.as_socket(), client)?;
    let net_description = describe_socket(wires.net.as_socket(), client)?;
    Ok(Lease {
        machine,
        folder: granted,
        wires,
        control_description,
        net_description,
    })
}

/// Name-surrogate reparse points redirect paths; cloud placeholders do not.
const REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;

/// Writable grants add creation rights; `FILE_DELETE_CHILD` would exclude ordinary Modify grants.
const fn rights_for(read_only: bool) -> u32 {
    let read = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES;
    if read_only {
        read
    } else {
        read | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY
    }
}

/// Opened as the client without `FILE_SHARE_DELETE`; the folder stays pinned until machine teardown.
struct Granted {
    handle: OwnedHandle,
    path: PathBuf,
}

impl Granted {
    /// Production calls must hold the client's token on this thread.
    fn open(folder: &Path, read_only: bool) -> Result<Self, Error> {
        let named = plain(folder.to_path_buf());
        let wide = wide(folder.as_os_str());
        // SAFETY: `wide` is NUL-terminated and alive; optional pointer arguments are null.
        let raw = unsafe {
            site!(
                Silent::ClientFolderOpen,
                CreateFileW(
                    wide.as_ptr(),
                    rights_for(read_only),
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    std::ptr::null_mut(),
                )
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(Error::FolderForbidden {
                path: named,
                read_only,
                cause: io::Error::last_os_error().to_string(),
            });
        }
        // SAFETY: a fresh handle this function owns.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };

        let unexamined = |cause: io::Error| Error::FolderUnexamined {
            path: named.clone(),
            cause: cause.to_string(),
        };
        let tag = attribute_tag(&handle).map_err(unexamined)?;
        judge(&named, tag)?;
        let path = final_path(&handle).map_err(unexamined)?;
        Ok(Self { handle, path })
    }

    /// Resolved under the client's token, in Windows' verbatim spelling.
    fn path(&self) -> &Path {
        &self.path
    }

    fn unchanged(&self) -> Result<(), Error> {
        let moved = || Error::FolderChanged(plain(self.path.clone()));
        let tag = attribute_tag(&self.handle).map_err(|_| moved())?;
        judge(&self.path, tag).map_err(|_| moved())?;
        match final_path(&self.handle) {
            Ok(now) if now == self.path => Ok(()),
            _ => Err(moved()),
        }
    }
}

/// # Safety
/// `pipe` must be a live, connected server end of a named pipe.
unsafe fn open_for_client(pipe: HANDLE, folder: &Path, read_only: bool) -> Result<Granted, Error> {
    // SAFETY: `pipe` is a connected server-end handle; impersonation lasts until
    // `RevertToSelf`, which the guard below performs on every path out.
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(Error::backend(
            HYPERVISOR,
            format!(
                "synod's machine service could not check who was asking: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    let guard = Impersonation;
    let granted = Granted::open(folder, read_only);
    drop(guard);
    granted
}

/// Refuse name surrogates: the access check covered the link, not its target.
fn judge(folder: &Path, tag: FILE_ATTRIBUTE_TAG_INFO) -> Result<(), Error> {
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && tag.ReparseTag & REPARSE_TAG_NAME_SURROGATE != 0
    {
        return Err(Error::FolderIsLink(folder.to_path_buf()));
    }
    if tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(Error::NotAFolder(folder.to_path_buf()));
    }
    Ok(())
}

fn attribute_tag(handle: &OwnedHandle) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    let mut tag = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: the handle has `FILE_READ_ATTRIBUTES`; `tag` matches the information class and size.
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

fn final_path(handle: &OwnedHandle) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: the handle is live, and `buffer` is writable for `capacity`
        // wide characters.
        let written = unsafe {
            site!(
                Silent::HeldFolderPath,
                GetFinalPathNameByHandleW(
                    handle.as_raw_handle(),
                    buffer.as_mut_ptr(),
                    capacity,
                    FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
                )
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

/// Revert the client's token on every exit, including unwinding.
struct Impersonation;

impl Drop for Impersonation {
    fn drop(&mut self) {
        // SAFETY: called exactly once, on a thread this module impersonated.
        unsafe { RevertToSelf() };
    }
}

/// The kernel supplies the client PID; the caller cannot choose it.
fn client_process(pipe: HANDLE) -> Result<u32, Error> {
    let mut pid = 0u32;
    // SAFETY: `pipe` is a connected server-end handle and `pid` a writable slot.
    if unsafe { GetNamedPipeClientProcessId(pipe, &raw mut pid) } == 0 {
        return Err(Error::backend(
            HYPERVISOR,
            format!(
                "synod's machine service could not identify the program that asked: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    Ok(pid)
}

/// Socket descriptions are valid only for the named client process.
fn describe_socket(socket: BorrowedSocket<'_>, pid: u32) -> Result<Vec<u8>, Error> {
    use windows_sys::Win32::Networking::WinSock::{SOCKET_ERROR, WSAPROTOCOL_INFOW};

    // SAFETY: `WSAPROTOCOL_INFOW` is a plain C structure of `Copy` fields, so a
    // zeroed one is a valid value to be written into.
    let mut info = unsafe { std::mem::zeroed::<WSAPROTOCOL_INFOW>() };
    // SAFETY: the socket is live, `pid` names the client, and `info` is a writable protocol structure.
    let duplicated = unsafe {
        windows_sys::Win32::Networking::WinSock::WSADuplicateSocketW(
            usize::try_from(socket.as_raw_socket()).expect("a SOCKET is pointer-sized"),
            pid,
            &raw mut info,
        )
    };
    if duplicated == SOCKET_ERROR {
        return Err(Error::backend(
            HYPERVISOR,
            format!(
                "a machine socket could not be handed to synod: {}",
                io::Error::last_os_error()
            ),
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

fn create_instance(first: bool) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_FIRST_PIPE_INSTANCE;

    let sddl = wide(PIPE_SDDL.as_ref());
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and alive; `descriptor` receives a `LocalAlloc` allocation.
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

    let name = wide(PIPE.as_ref());
    let mode = if first {
        // The first instance excludes a second broker from serving the same pipe name.
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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
#[allow(
    clippy::disallowed_macros,
    reason = "[test] libtest captures print macros"
)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    use vm_manager::Socket;

    use super::*;

    #[test]
    fn the_descriptor_parses() {
        drop(create_instance(false).expect("a pipe instance"));
    }

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

    /// Requests are read from `input`; replies accumulate apart, so reads never see them.
    struct Duplex {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn exchange(
        input: Vec<u8>,
        on_boot: impl FnMut(&Path, bool) -> Result<Lease, Error>,
    ) -> Vec<Reply> {
        let mut stream = Duplex {
            input: std::io::Cursor::new(input),
            output: Vec::new(),
        };
        serve_client(&mut stream, on_boot);
        let mut replies = Vec::new();
        let mut output = &stream.output[..];
        while let Some(reply) = frame::read(&mut output).expect("a well-formed reply") {
            replies.push(reply);
        }
        replies
    }

    fn framed(requests: &[Request]) -> Vec<u8> {
        let mut buffer = Vec::new();
        for request in requests {
            frame::write(&mut buffer, request).unwrap();
        }
        buffer
    }

    fn boot_request(folder: &Path) -> Request {
        Request::Boot {
            version: VERSION,
            folder: folder.to_path_buf(),
            read_only: true,
        }
    }

    /// What the fake machine saw of the broker's state at the moment it was asked.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Console {
            wires_closed: bool,
        },
        Shutdown {
            wires_closed: bool,
            folder_pinned: bool,
        },
    }

    /// A guest-side peer reads EOF exactly when the broker's copy of its wire is gone.
    fn closed(mut peer: &TcpStream) -> bool {
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        matches!(peer.read(&mut [0]), Ok(0))
    }

    struct Fake {
        peers: [TcpStream; 2],
        folder: PathBuf,
        seen: Arc<Mutex<Vec<Seen>>>,
        outcome: Result<(), Error>,
    }

    impl Fake {
        fn wires_closed(&self) -> bool {
            self.peers.iter().all(closed)
        }
    }

    impl Machine for Fake {
        fn dial_engine(&self) -> io::Result<Socket> {
            Err(io::ErrorKind::Unsupported.into())
        }

        fn connect_guest(&self, _: u32) -> io::Result<Socket> {
            Err(io::ErrorKind::Unsupported.into())
        }

        fn console(&self) -> GuestConsole {
            let wires_closed = self.wires_closed();
            self.seen
                .lock()
                .unwrap()
                .push(Seen::Console { wires_closed });
            GuestConsole::Unkept
        }

        fn shutdown(self: Box<Self>) -> Result<(), Error> {
            let wires_closed = self.wires_closed();
            let moved = self.folder.with_file_name("moved");
            let folder_pinned = std::fs::rename(&self.folder, moved).is_err();
            self.seen.lock().unwrap().push(Seen::Shutdown {
                wires_closed,
                folder_pinned,
            });
            self.outcome
        }
    }

    fn socket_pair() -> (Socket, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        (listener.accept().unwrap().0.into(), peer)
    }

    /// Boots a fake machine on the requested folder, pinned as in production.
    fn fake_boot(
        seen: &Arc<Mutex<Vec<Seen>>>,
        outcome: Result<(), Error>,
    ) -> impl FnMut(&Path, bool) -> Result<Lease, Error> {
        let seen = Arc::clone(seen);
        move |folder, read_only| {
            let (control, control_peer) = socket_pair();
            let (net, net_peer) = socket_pair();
            Ok(Lease {
                wires: Wires { control, net },
                machine: Box::new(Fake {
                    peers: [control_peer, net_peer],
                    folder: folder.to_path_buf(),
                    seen: Arc::clone(&seen),
                    outcome: outcome.clone(),
                }),
                folder: Granted::open(folder, read_only)?,
                control_description: b"control".to_vec(),
                net_description: b"net".to_vec(),
            })
        }
    }

    #[test]
    fn a_second_boot_on_one_connection_is_refused() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let replies = exchange(
            framed(&[boot_request(dir.path()), boot_request(dir.path())]),
            fake_boot(&Arc::default(), Ok(())),
        );
        let [
            Reply::Booted { control, net },
            Reply::Refused(Error::Backend { .. }),
        ] = &replies[..]
        else {
            panic!("{replies:?}")
        };
        assert_eq!((&control[..], &net[..]), (&b"control"[..], &b"net"[..]));
    }

    #[test]
    fn stop_closes_the_wires_then_shuts_down_then_releases_the_folder() {
        let root = tempfile::tempdir().expect("scratch folder");
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).expect("folder");
        let seen = Arc::default();
        let replies = exchange(
            framed(&[boot_request(&folder), Request::Stop]),
            fake_boot(&seen, Err(Error::NoBackend)),
        );
        assert!(
            matches!(
                &replies[..],
                [Reply::Booted { .. }, Reply::Stopped(Err(Error::NoBackend))]
            ),
            "{replies:?}"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [Seen::Shutdown {
                wires_closed: true,
                folder_pinned: true
            }]
        );
        std::fs::rename(&folder, root.path().join("moved")).expect("free once stopped");
    }

    #[test]
    fn adopting_drops_the_wires_without_a_reply_and_the_connection_goes_on() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let seen = Arc::default();
        let replies = exchange(
            framed(&[boot_request(dir.path()), Request::Adopted, Request::Console]),
            fake_boot(&seen, Ok(())),
        );
        assert!(
            matches!(
                &replies[..],
                [Reply::Booted { .. }, Reply::Console(GuestConsole::Unkept)]
            ),
            "{replies:?}"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [Seen::Console { wires_closed: true }]
        );
    }

    #[test]
    fn the_console_before_any_boot_is_empty() {
        let replies = exchange(framed(&[Request::Console]), |_, _| {
            panic!("no boot was requested")
        });
        assert!(
            matches!(
                &replies[..],
                [Reply::Console(GuestConsole::Kept { log: None, tail })] if tail.is_empty()
            ),
            "{replies:?}"
        );
    }

    #[test]
    fn a_boot_of_another_version_is_refused_before_boot() {
        let folder = PathBuf::from(r"C:\Users\secretary\Documents");
        let replies = exchange(
            framed(&[Request::Boot {
                version: VERSION + 1,
                folder,
                read_only: true,
            }]),
            |_, _| panic!("version skew must precede boot"),
        );
        assert!(
            matches!(
                &replies[..],
                [Reply::Refused(Error::VersionSkew { client, service: VERSION })]
                    if *client == VERSION + 1
            ),
            "{replies:?}"
        );
    }

    #[test]
    fn an_undecodable_boot_of_another_version_is_refused_and_the_connection_closed() {
        let body = br#"{"Boot":{"version":6,"folder":"old encoding","read_only":true}}"#;
        let mut input = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
        input.extend_from_slice(body);
        input.extend(framed(&[Request::Console]));
        let replies = exchange(input, |_, _| panic!("version skew must precede boot"));
        assert!(
            matches!(
                &replies[..],
                [Reply::Refused(Error::VersionSkew {
                    client: 6,
                    service: VERSION
                })]
            ),
            "the console request after the refusal is never answered: {replies:?}"
        );
    }

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

    #[test]
    fn only_a_real_folder_is_judged_grantable() {
        const JUNCTION: u32 = 0xA000_0003;
        const SYMLINK: u32 = 0xA000_000C;
        const CLOUD: u32 = 0x9000_001A;
        let folder = Path::new(r"C:\Users\secretary\Documents");
        let tagged = |attributes, tag| FILE_ATTRIBUTE_TAG_INFO {
            FileAttributes: attributes,
            ReparseTag: tag,
        };
        let link = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT;
        for tag in [JUNCTION, SYMLINK] {
            match judge(folder, tagged(link, tag)) {
                Err(Error::FolderIsLink(path)) => assert_eq!(path, folder),
                other => panic!("a link is refused as one: {other:?}"),
            }
        }
        judge(folder, tagged(link, CLOUD)).expect("an online-only folder is a folder");
        judge(folder, tagged(FILE_ATTRIBUTE_DIRECTORY, 0)).expect("a plain folder");
        match judge(folder, tagged(0x80, 0)) {
            Err(Error::NotAFolder(path)) => assert_eq!(path, folder),
            other => panic!("a file is refused as not a folder: {other:?}"),
        }
    }

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

    #[test]
    fn a_folder_one_may_only_read_is_refused_with_changes_allowed() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let restricted = Restricted::to(dir.path(), "D:P(A;OICI;FRFX;;;WD)");
        Granted::open(dir.path(), true).expect("readable, so granted read-only");
        let refused = Granted::open(dir.path(), false)
            .err()
            .expect("not writable, so refused with changes allowed");
        assert!(
            matches!(
                refused,
                Error::FolderForbidden {
                    read_only: false,
                    ..
                }
            ),
            "{refused:?}"
        );
        drop(restricted);
    }

    #[test]
    fn a_folder_one_may_not_read_is_refused() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let restricted = Restricted::to(dir.path(), "D:P(A;OICI;RC;;;WD)");
        for read_only in [true, false] {
            assert!(Granted::open(dir.path(), read_only).is_err(), "{read_only}");
        }
        drop(restricted);
    }

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
            let refused = Granted::open(&link, read_only)
                .err()
                .expect("a junction is refused");
            assert!(matches!(refused, Error::FolderIsLink(_)), "{refused:?}");
        }
    }

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

    #[test]
    fn a_file_is_refused() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"hello").expect("a file");
        let refused = Granted::open(&file, true).err().expect("refused");
        assert!(matches!(refused, Error::NotAFolder(_)), "{refused:?}");
    }

    /// Junction creation needs no symbolic-link privilege.
    fn junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// The owner can restore the DACL on drop so the temporary folder remains removable.
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
        let sddl = wide(sddl.as_ref());
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
        let path = wide(folder.as_os_str());
        // SAFETY: `path` is NUL-terminated and `descriptor` a valid descriptor.
        let set = unsafe { SetFileSecurityW(path.as_ptr(), DACL_SECURITY_INFORMATION, descriptor) };
        let cause = io::Error::last_os_error();
        // SAFETY: allocated by the conversion above and not used after this.
        unsafe { LocalFree(descriptor.cast()) };
        assert_ne!(set, 0, "{cause}");
    }

    /// A machine-layer refusal also proves the broker path; an installed service is left alone.
    #[test]
    fn the_broker_answers_a_client_over_a_real_pipe() {
        use vm_manager::broker::client::Brokered;

        if Brokered::available() {
            eprintln!("a machine broker is already installed on this computer; not displacing it");
            return;
        }
        std::thread::Builder::new()
            .name("test-broker".to_string())
            .spawn(|| {
                let _ = serve();
            })
            .expect("a thread to serve on");

        assert!(
            Brokered::available(),
            "the client must find the pipe the service just opened"
        );

        let folder = std::env::temp_dir().join("synod-broker-test-folder");
        std::fs::create_dir_all(&folder).expect("a folder to grant");
        let outcome = Brokered.boot(&MachineSpec {
            folder,
            read_only: false,
        });

        match outcome {
            Ok(Booted { machine, wires }) => {
                drop(wires);
                machine.shutdown().expect("the machine stops cleanly");
            }
            Err(refused) => assert!(
                matches!(
                    refused,
                    Error::Backend { .. }
                        | Error::NoBackend
                        | Error::NoGuestImage { .. }
                        | Error::NotConstructed { .. }
                        | Error::NotPermitted
                ),
                "the refusal must be the machine layer's, having crossed the pipe: {refused:?}"
            ),
        }
    }
}
