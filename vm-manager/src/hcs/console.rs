//! The guest's console, on a named pipe.
//!
//! `dev/docs/VM/SYNOD.md` §1 gives the daemon a console→host log, and that log
//! is the only thing that speaks during the seconds before a guest can speak
//! for itself.  Without it a boot that fails — a kernel that cannot find its
//! disks, an initramfs that refuses, a daemon that will not accept the command
//! line — is indistinguishable from a boot that is merely slow: both are a
//! timeout waiting for a control connection that never comes.  With it, the
//! reason is on synod's own output, in the guest's own words.
//!
//! The macOS backend gets this almost for free — a virtio console attached to
//! the host's `stdout`.  Hyper-V has no virtio, so the guest gets an emulated
//! serial port (`ttyS0`) whose far end is a named pipe: the compute service
//! connects to it as a *client* when the machine starts, which means the pipe's
//! server end must exist first.  Hence the order in [`Hypervisor::boot`](crate::Hypervisor::boot):
//! create the pipe, create the machine, start it, then read.
//!
//! # Waking a pump that is waiting for a client that will never come
//!
//! A server end parked in `ConnectNamedPipe` is parked until *something*
//! connects.  If the machine fails to start there is no client, and the pump
//! thread would sit there for the life of the process.  The remedy is the
//! standard one and needs no handle sharing across threads: [`Console::wake`]
//! connects to the pipe itself and immediately closes, which is a connection as
//! far as the parked server is concerned.  The pump then reads an immediate
//! end-of-file and exits.  This is why a [`Console`] keeps only the pipe's
//! *name* after handing its server end to the pump.

use std::io::Write;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// The pipe's buffer, in bytes.  A console produces lines, not bulk data, so
/// this is sized to be comfortable rather than fast.
const PIPE_BUFFER: u32 = 8 * 1024;

/// `PIPE_ACCESS_DUPLEX`.  The pipe is opened both ways although synod only ever
/// reads: the emulated serial port is a two-way device, and a service that
/// opens it for read/write must find a pipe that permits both or the machine
/// fails to start with a message about the COM port rather than about this.
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;

/// One machine's console: the pipe it writes to, and the name by which a
/// stuck reader can be woken.
#[derive(Debug)]
pub(super) struct Console {
    name: String,
}

impl Console {
    /// Create the server end of one machine's console pipe.
    ///
    /// Must happen before the machine is created, since the machine's document
    /// names this pipe and the service dials it at start.
    ///
    /// # Errors
    /// Returns a sentence if Windows would not create the pipe.  A console is a
    /// diagnostic, so [`Hypervisor::boot`](crate::Hypervisor::boot) treats this as a loss to report
    /// rather than a reason to refuse a session.
    pub(super) fn create(id: &str) -> Result<Self, String> {
        let name = format!(r"\\.\pipe\synod-console-{id}");
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated pipe name alive for the call; one
        // instance, byte mode, blocking, default timeout, and the service's own
        // default security (null), which grants the creating user.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(format!(
                "the guest console pipe {name} could not be created: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: a fresh pipe handle this function owns; ownership moves to the
        // `OwnedHandle` the pump thread takes, and is closed exactly once there.
        let server = unsafe { OwnedHandle::from_raw_handle(handle) };
        spawn_pump(server, name.clone());
        Ok(Self { name })
    }

    /// The name the machine's document must carry.
    pub(super) fn pipe(&self) -> &str {
        &self.name
    }

    /// Unblock a pump still waiting for the machine to connect.
    ///
    /// Called on every teardown path, including the one where the machine never
    /// started.  Harmless when the pump has already moved on: the client handle
    /// is opened and dropped, and a pipe with no server left to accept it
    /// simply fails to open.
    pub(super) fn wake(&self) {
        let wide: Vec<u16> = self.name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated pipe name alive for the call.  A
        // zero access mask asks for a connection without read or write rights,
        // which is all a wake-up needs, and the returned handle is adopted so it
        // closes immediately.
        let client = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if client != INVALID_HANDLE_VALUE && !client.is_null() {
            // SAFETY: a fresh handle owned here and dropped at once, which is
            // the whole point — the connection existing is the signal.
            drop(unsafe { OwnedHandle::from_raw_handle(client) });
        }
    }
}

/// Read the guest's console onto synod's own output until the machine stops.
///
/// The thread is never joined: it ends when the machine's end of the pipe
/// closes (a stopped machine), or when [`Console::wake`] gives it the
/// end-of-file that lets a never-started machine's pump retire.
fn spawn_pump(server: OwnedHandle, name: String) {
    std::thread::Builder::new()
        .name("synod-guest-console".to_string())
        .spawn(move || {
            use std::os::windows::io::AsRawHandle;
            let handle: HANDLE = server.as_raw_handle();
            // SAFETY: `handle` is the live server end this thread owns.  A null
            // overlapped pointer is the blocking form, which is what a
            // dedicated thread wants.
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            // A client that connected between `CreateNamedPipeW` and here is
            // reported as a failure with this specific code, and is success.
            if connected == 0
                && std::io::Error::last_os_error().raw_os_error()
                    != Some(ERROR_PIPE_CONNECTED.cast_signed())
            {
                return;
            }

            let mut buffer = [0u8; PIPE_BUFFER as usize];
            let mut out = std::io::stdout();
            loop {
                let mut read = 0u32;
                // SAFETY: `buffer` is writable for the length passed, `read` is
                // a writable slot, and the null overlapped pointer keeps the
                // read blocking.
                let ok = unsafe {
                    ReadFile(
                        handle,
                        buffer.as_mut_ptr(),
                        PIPE_BUFFER,
                        &raw mut read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || read == 0 {
                    break;
                }
                // A console line the guest wrote is the guest's own ink, so it
                // goes to synod's output as bytes and is never parsed here.
                if out.write_all(&buffer[..read as usize]).is_err() {
                    break;
                }
                let _ = out.flush();
            }
            drop(name);
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console names a pipe under the local pipe namespace, per machine, so
    /// two sessions never share one.
    #[test]
    fn each_machine_gets_its_own_pipe() {
        let a = Console::create("0000-a").expect("a pipe is created");
        let b = Console::create("0000-b").expect("a second pipe is created");
        assert!(a.pipe().starts_with(r"\\.\pipe\synod-console-"));
        assert_ne!(a.pipe(), b.pipe());
        a.wake();
        b.wake();
    }

    /// The same name twice is refused rather than silently shared: a second
    /// instance of an existing pipe would have two machines writing one
    /// console.
    #[test]
    fn a_name_already_in_use_is_refused() {
        let held = Console::create("0000-clash").expect("first");
        let err = Console::create("0000-clash").expect_err("the name is taken");
        assert!(err.contains("could not be created"), "{err}");
        held.wake();
    }

    /// Waking a pipe nobody is serving is a no-op, not a panic — the teardown
    /// path calls it unconditionally.
    #[test]
    fn waking_an_absent_pipe_is_harmless() {
        Console {
            name: r"\\.\pipe\synod-console-never-existed".to_string(),
        }
        .wake();
    }
}
