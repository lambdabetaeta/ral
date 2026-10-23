//! A real engine on a local socket, shared by wire tests.

use ral_core::carrier::WireTransport;
use ral_core::protocol::channel::Liveness;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

pub fn spawn_engine_on(guest: &UnixStream) -> std::process::Child {
    let guest_fd = guest.as_raw_fd();
    let mut cmd = std::process::Command::new(std::env::current_exe().expect("current exe"));
    cmd.arg(ral_core::Role::Engine.flag());
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    // SAFETY: runs between fork and exec, calling only async-signal-safe
    // `dup2`/`close`, with no allocation and no locking.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if libc::dup2(guest_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if guest_fd != 3 {
                libc::close(guest_fd);
            }
            Ok(())
        });
    }
    cmd.spawn().expect("spawn engine child")
}

pub fn engine_over_socketpair(liveness: Liveness) -> (WireTransport, std::process::Child) {
    let (host, guest) = UnixStream::pair().expect("socketpair");
    let child = spawn_engine_on(&guest);
    drop(guest);
    (
        WireTransport::adopt(host, liveness).expect("adopt host stream"),
        child,
    )
}
