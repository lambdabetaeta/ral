//! IPC channel over a Windows named pipe.
//!
//! [`IpcChannel`] owns the parent side of a duplex named pipe with a
//! single allowed instance, a unique name per spawn, a first-instance
//! claim, and a per-user DACL — so a pre-existing pipe of the same name
//! cannot be squatted into the channel.  The client (child-side) handle
//! is an [`IpcEndpoint`], confined at construction and lent inheritable
//! only across the spawn.
//!
//! Wire framing is the same length-prefixed JSON discipline as the Unix
//! transport — `crate::subprocess_codec::{write_frame, read_frame}` —
//! so the [`crate::child_eval`] types serialize identically.

use super::IpcEndpoint;
use crate::child_eval::{ChildEvalRequest, ChildEvalResponse};
use crate::types::Settled;
use std::ffi::OsStr;
use std::io::{self, BufReader, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
    InitializeSecurityDescriptor, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `SECURITY_DESCRIPTOR_REVISION`, not re-exported under that name from
/// `Win32::Security` in `windows-sys` 0.61.
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

/// Env var name that carries the child-side pipe handle (as a raw
/// integer-string) into the re-exec'd ral process.  The child reads
/// this, casts it back to `HANDLE`, and serves one request.
pub const IPC_HANDLE_ENV: &str = "RAL_SANDBOX_IPC_HANDLE";

/// `Read`/`Write` adapter over a raw pipe `HANDLE`.  Wraps
/// `ReadFile`/`WriteFile`; treats `ERROR_BROKEN_PIPE`/`ERROR_HANDLE_EOF`
/// on read as orderly close (`Ok(0)`) so the framing codec sees EOF.
/// Borrows the handle — ownership and closing belong to the holder
/// (the parent's [`NamedPipe`], the child's `OwnedHandle`).  `Copy` so
/// one handle can back both a read half and a write half; reads run to
/// completion before any write.
#[derive(Clone, Copy)]
pub(super) struct PipeIo {
    pub(super) handle: HANDLE,
}

impl Read for PipeIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read: u32 = 0;
        let cap = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                cap,
                &mut read as *mut u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            const ERROR_BROKEN_PIPE: u32 = 109;
            const ERROR_HANDLE_EOF: u32 = 38;
            if err == ERROR_BROKEN_PIPE || err == ERROR_HANDLE_EOF {
                return Ok(0);
            }
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        Ok(read as usize)
    }
}

impl Write for PipeIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written: u32 = 0;
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                len,
                &mut written as *mut u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ));
        }
        Ok(written as usize)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Parent side of the IPC channel: a connected named-pipe server
/// handle wrapped for sync read/write.  Single duplex instance per
/// spawn, with a unique suffix in the pipe name to keep concurrent ral
/// grants from colliding.
pub struct IpcChannel {
    pipe: NamedPipe,
}

/// Owned named-pipe server handle; closed on drop.  Reads/writes go
/// through the shared [`PipeIo`] adapter.
struct NamedPipe {
    io: PipeIo,
}

impl Drop for NamedPipe {
    fn drop(&mut self) {
        if self.io.handle != INVALID_HANDLE_VALUE && !self.io.handle.is_null() {
            unsafe { CloseHandle(self.io.handle) };
        }
    }
}

impl Read for NamedPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for NamedPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl IpcChannel {
    /// Create a fresh named pipe with a unique name, return the connected
    /// server side plus the client-side [`IpcEndpoint`].  The server
    /// claims the first instance and carries a per-user DACL, so a
    /// pre-existing pipe of the same name fails creation rather than
    /// being shared.  The endpoint is born non-inheritable; inheritance
    /// is opened only transiently inside [`IpcEndpoint::lend_for_spawn`].
    pub fn open_pair() -> io::Result<(Self, IpcEndpoint)> {
        let pid = std::process::id();
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!("\\\\.\\pipe\\ral-sandbox-{pid}-{ns:032x}");
        let wide: Vec<u16> = OsStr::new(&name).encode_wide().chain([0]).collect();

        // A DACL granting only the current user closes the pipe to other
        // principals; the backing storage must outlive CreateNamedPipeW.
        let mut security = PipeSecurity::current_user()?;
        let server = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                64 * 1024,
                64 * 1024,
                0,
                security.attributes(),
            )
        };
        if server == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        // Open the client side from the parent born non-inheritable;
        // IpcEndpoint::lend_for_spawn opens inheritance transiently around
        // the spawn that hands it to the child.
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 0,
        };
        let client = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                &mut sa as *mut _,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if client == INVALID_HANDLE_VALUE {
            let err = io::Error::last_os_error();
            unsafe { CloseHandle(server) };
            return Err(err);
        }

        // ConnectNamedPipe completes the handshake; on a pipe whose
        // client has already opened it returns 0 + ERROR_PIPE_CONNECTED
        // which is the success path here.
        let connect_ok = unsafe { ConnectNamedPipe(server, std::ptr::null_mut()) };
        if connect_ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_PIPE_CONNECTED {
                unsafe {
                    CloseHandle(server);
                    CloseHandle(client);
                }
                return Err(io::Error::from_raw_os_error(err as i32));
            }
        }

        // Safety: `client` is a freshly-created handle that the parent
        // process exclusively owns; `into_raw_handle` transfers it to the
        // endpoint, whose constructor clears `HANDLE_FLAG_INHERIT`.
        let owned = unsafe { OwnedHandle::from_raw_handle(client as RawHandle) };
        let endpoint = match unsafe { IpcEndpoint::adopt(owned.into_raw_handle()) } {
            Ok(endpoint) => endpoint,
            Err(err) => {
                unsafe { CloseHandle(server) };
                return Err(err);
            }
        };
        Ok((
            IpcChannel {
                pipe: NamedPipe {
                    io: PipeIo { handle: server },
                },
            },
            endpoint,
        ))
    }

    /// Send the eval request and read back the child's single response,
    /// rejecting any frame whose confinement token does not match the one
    /// minted for this re-exec.  Mirrors the Unix transport's `drive`.
    ///
    /// The named pipe is one duplex handle; [`PipeIo`] is `Copy`, so a
    /// copy backs the writer and a buffered reader over a second copy
    /// backs the reader.  The exchange writes the request fully before
    /// reading, so the two halves never interleave on the wire.
    pub fn drive(self, request: &ChildEvalRequest) -> Settled<ChildEvalResponse> {
        let mut writer = self.pipe.io;
        let mut reader = BufReader::new(self.pipe.io);
        super::drive_exchange(&mut writer, &mut reader, request)
    }
}

/// A self-allocated security descriptor whose DACL grants the current
/// user full access to the pipe and no one else.  Owns the SID buffer,
/// the ACL buffer, and the descriptor so they outlive the
/// `CreateNamedPipeW` call that references them.
struct PipeSecurity {
    attributes: SECURITY_ATTRIBUTES,
    // Held so the security descriptor `attributes` points at, the DACL
    // that descriptor names, and the SID that ACL names, all stay alive
    // for as long as the attributes are read.
    _descriptor: Box<SECURITY_DESCRIPTOR>,
    _acl: Vec<u8>,
    _token_user: Vec<u8>,
}

impl PipeSecurity {
    /// Build a descriptor granting `GENERIC_ALL` to the current user SID.
    fn current_user() -> io::Result<Self> {
        let mut token_user = current_user_token_information()?;
        let sid = unsafe { (*(token_user.as_mut_ptr() as *mut TOKEN_USER)).User.Sid };

        // ACL sized for its header plus one access-allowed ACE: ACE header
        // (4) + access mask (4) + the SID inline (its full length), with a
        // little headroom.  `AddAccessAllowedAce` copies the SID in and
        // sizes the ACE itself, so over-allocating is safe.
        let acl_len: u32 = (std::mem::size_of::<ACL>() + 8 + sid_length(sid) as usize + 8) as u32;
        let mut acl: Vec<u8> = vec![0u8; acl_len as usize];
        let acl_ptr = acl.as_mut_ptr() as *mut ACL;
        if unsafe { InitializeAcl(acl_ptr, acl_len, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { AddAccessAllowedAce(acl_ptr, ACL_REVISION, GENERIC_ALL, sid) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut descriptor: Box<SECURITY_DESCRIPTOR> = Box::new(unsafe { std::mem::zeroed() });
        let descriptor_ptr =
            descriptor.as_mut() as *mut SECURITY_DESCRIPTOR as *mut core::ffi::c_void;
        if unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        // Attach the DACL: present, not defaulted, so the kernel enforces
        // exactly this ACE and denies every principal it omits.
        if unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor_ptr,
            bInheritHandle: 0,
        };
        Ok(Self {
            attributes,
            _descriptor: descriptor,
            _acl: acl,
            _token_user: token_user,
        })
    }

    /// Pointer to the security attributes, valid while `self` lives.
    fn attributes(&mut self) -> *const SECURITY_ATTRIBUTES {
        &self.attributes as *const SECURITY_ATTRIBUTES
    }
}

/// Read this process's `TOKEN_USER` into a fresh byte buffer; the SID it
/// names lives inside the returned storage.
fn current_user_token_information() -> io::Result<Vec<u8>> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token as *mut HANDLE) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // First call sizes the buffer (fails with ERROR_INSUFFICIENT_BUFFER),
    // the second fills it.
    let mut needed: u32 = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed as *mut u32,
        );
    }
    let mut buf: Vec<u8> = vec![0u8; needed as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            needed,
            &mut needed as *mut u32,
        )
    };
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

/// Byte length of a SID via `GetLengthSid`.
fn sid_length(sid: PSID) -> u32 {
    unsafe { GetLengthSid(sid) }
}
